// Std
use std::{
    borrow::Cow,
    cmp,
    collections::BTreeMap,
    io::{self, Write},
    usize,
};

// Internal
use crate::{
    build::{App, AppSettings, Arg, ArgSettings},
    output::{fmt::Colorizer, Usage},
    parse::{
        errors::{Error, Result as ClapResult},
        Parser,
    },
    util::VecMap,
    INTERNAL_ERROR_MSG,
};

// Third party
use indexmap::IndexSet;
use unicode_width::UnicodeWidthStr;

pub(crate) fn dimensions() -> Option<(usize, usize)> {
    #[cfg(not(feature = "wrap_help"))]
    return None;

    #[cfg(feature = "wrap_help")]
    terminal_size::terminal_size().map(|(w, h)| (w.0.into(), h.0.into()))
}

fn str_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

const TAB: &str = "    ";

pub(crate) enum HelpWriter<'w> {
    Normal(&'w mut dyn Write),
    Buffer(&'w mut Colorizer),
}

impl HelpWriter<'_> {
    fn finish(&mut self) -> io::Result<()> {
        match self {
            HelpWriter::Normal(ref mut writer) => writer.flush(),
            HelpWriter::Buffer(ref mut colorizer) => colorizer.print(),
        }
    }
}

/// `clap` Help Writer.
///
/// Wraps a writer stream providing different methods to generate help for `clap` objects.
pub(crate) struct Help<'b, 'c, 'd, 'w> {
    writer: HelpWriter<'w>,
    parser: &'d Parser<'b, 'c>,
    next_line_help: bool,
    hide_pv: bool,
    term_w: usize,
    longest: usize,
    force_next_line: bool,
    use_long: bool,
}

// Public Functions
impl<'b, 'c, 'd, 'w> Help<'b, 'c, 'd, 'w> {
    /// Create a new `Help` instance.
    pub(crate) fn new(w: HelpWriter<'w>, parser: &'d Parser<'b, 'c>, use_long: bool) -> Self {
        debug!("Help::new");
        let term_w = match parser.app.term_w {
            Some(0) => usize::MAX,
            Some(w) => w,
            None => cmp::min(
                dimensions().map_or(100, |(w, _)| w),
                match parser.app.max_w {
                    None | Some(0) => usize::MAX,
                    Some(mw) => mw,
                },
            ),
        };
        let nlh = parser.is_set(AppSettings::NextLineHelp);
        let hide_pv = parser.is_set(AppSettings::HidePossibleValuesInHelp);

        Help {
            writer: w,
            parser,
            next_line_help: nlh,
            hide_pv,
            term_w,
            longest: 0,
            force_next_line: false,
            use_long,
        }
    }

    /// Writes the parser help to the wrapped stream.
    pub(crate) fn write_help(&mut self) -> ClapResult<()> {
        debug!("Help::write_help");

        if let Some(h) = self.parser.app.help_str {
            self.none(h).map_err(Error::from)?;
        } else if let Some(tmpl) = self.parser.app.template {
            self.write_templated_help(tmpl)?;
        } else {
            self.write_default_help()?;
        }

        self.none("\n")?;

        Ok(())
    }
}

macro_rules! write_method {
    ($_self:ident, $msg:ident, $meth:ident) => {
        match &mut $_self.writer {
            HelpWriter::Buffer(c) => {
                c.$meth($msg);
                Ok(())
            }
            HelpWriter::Normal(w) => write!(w, "{}", $msg),
        }
    };
}

macro_rules! write_nspaces {
    ($_self:ident, $num:expr) => {{
        debug!("Help::write_nspaces!: num={}", $num);
        for _ in 0..$num {
            $_self.none(" ")?;
        }
    }};
}

// Methods to write Arg help.
impl<'b, 'c, 'd, 'w> Help<'b, 'c, 'd, 'w> {
    fn good(&mut self, msg: &str) -> io::Result<()> {
        write_method!(self, msg, good)
    }

    fn warning(&mut self, msg: &str) -> io::Result<()> {
        write_method!(self, msg, warning)
    }

    fn none(&mut self, msg: &str) -> io::Result<()> {
        write_method!(self, msg, none)
    }

    /// Writes help for each argument in the order they were declared to the wrapped stream.
    fn write_args_unsorted(&mut self, args: &[&Arg<'b>]) -> io::Result<()> {
        debug!("Help::write_args_unsorted");
        // The shortest an arg can legally be is 2 (i.e. '-x')
        self.longest = 2;
        let mut arg_v = Vec::with_capacity(10);
        let use_long = self.use_long;
        for arg in args.iter().filter(|arg| should_show_arg(use_long, *arg)) {
            if arg.longest_filter() {
                self.longest = cmp::max(self.longest, str_width(arg.to_string().as_str()));
            }
            arg_v.push(arg)
        }
        let mut first = true;
        let arg_c = arg_v.len();
        for (i, arg) in arg_v.iter().enumerate() {
            if first {
                first = false;
            } else {
                self.none("\n")?;
            }
            self.write_arg(arg, i < arg_c)?;
        }
        Ok(())
    }

    /// Sorts arguments by length and display order and write their help to the wrapped stream.
    fn write_args(&mut self, args: &[&Arg<'b>]) -> io::Result<()> {
        debug!("Help::write_args");
        // The shortest an arg can legally be is 2 (i.e. '-x')
        self.longest = 2;
        let mut ord_m = VecMap::new();
        let use_long = self.use_long;
        // Determine the longest
        for arg in args.iter().filter(|arg| {
            // If it's NextLineHelp we don't care to compute how long it is because it may be
            // NextLineHelp on purpose simply *because* it's so long and would throw off all other
            // args alignment
            should_show_arg(use_long, *arg)
        }) {
            if arg.longest_filter() {
                debug!("Help::write_args: Current Longest...{}", self.longest);
                self.longest = cmp::max(self.longest, str_width(arg.to_string().as_str()));
                debug!("Help::write_args: New Longest...{}", self.longest);
            }
            let btm = ord_m.entry(arg.disp_ord).or_insert(BTreeMap::new());
            // We use name here for alphabetic sorting
            // @TODO @maybe perhaps we could do some sort of ordering off of keys?
            btm.insert(arg.name, arg);
        }
        let mut first = true;
        for btm in ord_m.values() {
            for arg in btm.values() {
                if first {
                    first = false;
                } else {
                    self.none("\n")?;
                }
                self.write_arg(arg, false)?;
            }
        }
        Ok(())
    }

    /// Writes help for an argument to the wrapped stream.
    fn write_arg(&mut self, arg: &Arg<'c>, prevent_nlh: bool) -> io::Result<()> {
        debug!("Help::write_arg");
        self.short(arg)?;
        self.long(arg)?;
        let spec_vals = self.val(arg)?;
        self.help(arg, &*spec_vals, prevent_nlh)?;
        Ok(())
    }

    /// Writes argument's short command to the wrapped stream.
    fn short(&mut self, arg: &Arg<'c>) -> io::Result<()> {
        debug!("Help::short");

        self.none(TAB)?;

        if let Some(s) = arg.short {
            self.good(&format!("-{}", s))
        } else if arg.has_switch() {
            self.none(TAB)
        } else {
            Ok(())
        }
    }

    /// Writes argument's long command to the wrapped stream.
    fn long(&mut self, arg: &Arg<'c>) -> io::Result<()> {
        debug!("Help::long");
        if !arg.has_switch() {
            return Ok(());
        }
        if arg.is_set(ArgSettings::TakesValue) {
            if let Some(l) = arg.long {
                if arg.short.is_some() {
                    self.none(", ")?;
                }
                self.good(&format!("--{}", l))?
            }

            let sep = if arg.is_set(ArgSettings::RequireEquals) {
                "="
            } else {
                " "
            };
            self.none(sep)?;
        } else if let Some(l) = arg.long {
            if arg.short.is_some() {
                self.none(", ")?;
            }
            self.good(&format!("--{}", l))?;
        }
        Ok(())
    }

    /// Writes argument's possible values to the wrapped stream.
    fn val(&mut self, arg: &Arg<'c>) -> Result<String, io::Error> {
        debug!("Help::val: arg={}", arg.name);
        let mult =
            arg.is_set(ArgSettings::MultipleValues) || arg.is_set(ArgSettings::MultipleOccurrences);
        if arg.is_set(ArgSettings::TakesValue) || arg.index.is_some() {
            let delim = if arg.is_set(ArgSettings::RequireDelimiter) {
                arg.val_delim.expect(INTERNAL_ERROR_MSG)
            } else {
                ' '
            };
            if !arg.val_names.is_empty() {
                let mut it = arg.val_names.iter().peekable();
                while let Some((_, val)) = it.next() {
                    self.good(&format!("<{}>", val))?;
                    if it.peek().is_some() {
                        self.none(&delim.to_string())?;
                    }
                }
                let num = arg.val_names.len();
                if mult && num == 1 {
                    self.good("...")?;
                }
            } else if let Some(num) = arg.num_vals {
                let mut it = (0..num).peekable();
                while let Some(_) = it.next() {
                    self.good(&format!("<{}>", arg.name))?;
                    if it.peek().is_some() {
                        self.none(&delim.to_string())?;
                    }
                }
                if mult && num == 1 {
                    self.good("...")?;
                }
            } else if arg.has_switch() {
                self.good(&format!("<{}>", arg.name))?;
                if mult {
                    self.good("...")?;
                }
            } else {
                self.good(&arg.to_string())?;
            }
        }

        let spec_vals = self.spec_vals(arg);
        let h = arg.about.unwrap_or("");
        let h_w = str_width(h) + str_width(&*spec_vals);
        let nlh = self.next_line_help || arg.is_set(ArgSettings::NextLineHelp);
        let taken = self.longest + 12;
        self.force_next_line = !nlh
            && self.term_w >= taken
            && (taken as f32 / self.term_w as f32) > 0.40
            && h_w > (self.term_w - taken);

        debug!("Help::val: Has switch...");
        if arg.has_switch() {
            debug!("Yes");
            debug!("Help::val: force_next_line...{:?}", self.force_next_line);
            debug!("Help::val: nlh...{:?}", nlh);
            debug!("Help::val: taken...{}", taken);
            debug!(
                "val: help_width > (width - taken)...{} > ({} - {})",
                h_w, self.term_w, taken
            );
            debug!("Help::val: longest...{}", self.longest);
            debug!("Help::val: next_line...");
            if !(nlh || self.force_next_line) {
                debug!("No");
                let self_len = str_width(arg.to_string().as_str());
                // subtract ourself
                let mut spcs = self.longest - self_len;
                // Since we're writing spaces from the tab point we first need to know if we
                // had a long and short, or just short
                if arg.long.is_some() {
                    // Only account 4 after the val
                    spcs += 4;
                } else {
                    // Only account for ', --' + 4 after the val
                    spcs += 8;
                }

                write_nspaces!(self, spcs);
            } else {
                debug!("Yes");
            }
        } else if !(nlh || self.force_next_line) {
            debug!("No, and not next_line");
            write_nspaces!(
                self,
                self.longest + 4 - (str_width(arg.to_string().as_str()))
            );
        } else {
            debug!("No");
        }
        Ok(spec_vals)
    }

    fn write_before_after_help(&mut self, h: &str) -> io::Result<()> {
        debug!("Help::write_before_after_help");
        let mut help = String::from(h);
        // determine if our help fits or needs to wrap
        debug!(
            "Help::write_before_after_help: Term width...{}",
            self.term_w
        );
        let too_long = str_width(h) >= self.term_w;

        debug!("Help::write_before_after_help: Too long...");
        if too_long {
            debug!("Yes");
            debug!("Help::write_before_after_help: help: {}", help);
            debug!(
                "Help::write_before_after_help: help width: {}",
                str_width(&*help)
            );
            // Determine how many newlines we need to insert
            debug!(
                "Help::write_before_after_help: Usable space: {}",
                self.term_w
            );
            help = wrap_help(&help, self.term_w);
        } else {
            debug!("No");
        }
        self.none(&help)?;
        Ok(())
    }

    /// Writes argument's help to the wrapped stream.
    fn help(&mut self, arg: &Arg<'c>, spec_vals: &str, prevent_nlh: bool) -> io::Result<()> {
        debug!("Help::help");
        let h = if self.use_long {
            arg.long_about.unwrap_or_else(|| arg.about.unwrap_or(""))
        } else {
            arg.about.unwrap_or_else(|| arg.long_about.unwrap_or(""))
        };
        let mut help = String::from(h) + spec_vals;
        let nlh = self.next_line_help || arg.is_set(ArgSettings::NextLineHelp) || self.use_long;
        debug!("Help::help: Next Line...{:?}", nlh);

        let spcs = if nlh || self.force_next_line {
            12 // "tab" * 3
        } else {
            self.longest + 12
        };

        let too_long = spcs + str_width(h) + str_width(&*spec_vals) >= self.term_w;

        // Is help on next line, if so then indent
        if nlh || self.force_next_line {
            self.none(&format!("\n{}{}{}", TAB, TAB, TAB))?;
        }

        debug!("Help::help: Too long...");
        if too_long && spcs <= self.term_w {
            debug!("Yes");
            debug!("Help::help: help...{}", help);
            debug!("Help::help: help width...{}", str_width(&*help));
            // Determine how many newlines we need to insert
            let avail_chars = self.term_w - spcs;
            debug!("Help::help: Usable space...{}", avail_chars);
            help = wrap_help(&help, avail_chars);
        } else {
            debug!("No");
        }
        if let Some(part) = help.lines().next() {
            self.none(part)?;
        }
        for part in help.lines().skip(1) {
            self.none("\n")?;
            if nlh || self.force_next_line {
                self.none(&format!("{}{}{}", TAB, TAB, TAB))?;
            } else if arg.has_switch() {
                write_nspaces!(self, self.longest + 12);
            } else {
                write_nspaces!(self, self.longest + 8);
            }
            self.none(part)?;
        }
        if !prevent_nlh && !help.contains('\n') && (nlh || self.force_next_line) {
            self.none("\n")?;
        }
        Ok(())
    }

    fn spec_vals(&self, a: &Arg) -> String {
        debug!("Help::spec_vals: a={}", a);
        let mut spec_vals = vec![];
        if let Some(ref env) = a.env {
            debug!(
                "Help::spec_vals: Found environment variable...[{:?}:{:?}]",
                env.0, env.1
            );
            let env_val = if !a.is_set(ArgSettings::HideEnvValues) {
                format!(
                    "={}",
                    env.1
                        .as_ref()
                        .map_or(Cow::Borrowed(""), |val| val.to_string_lossy())
                )
            } else {
                String::new()
            };
            let env_info = format!(" [env: {}{}]", env.0.to_string_lossy(), env_val);
            spec_vals.push(env_info);
        }
        if !a.is_set(ArgSettings::HideDefaultValue) && !a.default_vals.is_empty() {
            debug!(
                "Help::spec_vals: Found default value...[{:?}]",
                a.default_vals
            );

            let pvs = a
                .default_vals
                .iter()
                .map(|&pvs| pvs.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");

            spec_vals.push(format!(" [default: {}]", pvs));
        }
        if !a.aliases.is_empty() {
            debug!("Help::spec_vals: Found aliases...{:?}", a.aliases);

            let als = a
                .aliases
                .iter()
                .filter(|&als| als.1) // visible
                .map(|&als| als.0) // name
                .collect::<Vec<_>>()
                .join(", ");

            if !als.is_empty() {
                spec_vals.push(format!(" [aliases: {}]", als));
            }
        }

        if !a.short_aliases.is_empty() {
            debug!(
                "Help::spec_vals: Found short aliases...{:?}",
                a.short_aliases
            );

            let als = a
                .short_aliases
                .iter()
                .filter(|&als| als.1) // visible
                .map(|&als| als.0.to_string()) // name
                .collect::<Vec<_>>()
                .join(", ");

            if !als.is_empty() {
                spec_vals.push(format!("[short aliases: {}]", als));
            }
        }

        if !self.hide_pv
            && !a.is_set(ArgSettings::HidePossibleValues)
            && !a.possible_vals.is_empty()
        {
            debug!(
                "Help::spec_vals: Found possible vals...{:?}",
                a.possible_vals
            );

            spec_vals.push(format!(
                " [possible values: {}]",
                a.possible_vals.join(", ")
            ));
        }
        spec_vals.join(" ")
    }
}

/// Methods to write a single subcommand
impl<'b, 'c, 'd, 'w> Help<'b, 'c, 'd, 'w> {
    fn write_subcommand(&mut self, app: &App<'b>) -> io::Result<()> {
        debug!("Help::write_subcommand");
        self.none(TAB)?;
        self.good(&app.name)?;
        let spec_vals = self.sc_val(app)?;
        self.sc_help(app, &*spec_vals)?;
        Ok(())
    }

    fn sc_val(&mut self, app: &App<'b>) -> Result<String, io::Error> {
        debug!("Help::sc_val: app={}", app.name);
        let spec_vals = self.sc_spec_vals(app);
        let h = app.about.unwrap_or("");
        let h_w = str_width(h) + str_width(&*spec_vals);
        let nlh = self.next_line_help;
        let taken = self.longest + 12;
        self.force_next_line = !nlh
            && self.term_w >= taken
            && (taken as f32 / self.term_w as f32) > 0.40
            && h_w > (self.term_w - taken);

        if !(nlh || self.force_next_line) {
            write_nspaces!(self, self.longest + 4 - (str_width(&app.name)));
        }
        Ok(spec_vals)
    }

    fn sc_spec_vals(&self, a: &App) -> String {
        debug!("Help::sc_spec_vals: a={}", a.name);
        let mut spec_vals = vec![];
        if !a.aliases.is_empty() {
            debug!("Help::spec_vals: Found aliases...{:?}", a.aliases);

            let als = a
                .aliases
                .iter()
                .filter(|&als| als.1) // visible
                .map(|&als| als.0) // name
                .collect::<Vec<_>>()
                .join(", ");

            if !als.is_empty() {
                spec_vals.push(format!(" [aliases: {}]", als));
            }
        }
        spec_vals.join(" ")
    }

    fn sc_help(&mut self, app: &App<'b>, spec_vals: &str) -> io::Result<()> {
        debug!("Help::sc_help");
        let h = if self.use_long {
            app.long_about.unwrap_or_else(|| app.about.unwrap_or(""))
        } else {
            app.about.unwrap_or_else(|| app.long_about.unwrap_or(""))
        };
        let mut help = String::from(h) + spec_vals;
        let nlh = self.next_line_help || self.use_long;
        debug!("Help::sc_help: Next Line...{:?}", nlh);

        let spcs = if nlh || self.force_next_line {
            12 // "tab" * 3
        } else {
            self.longest + 12
        };

        let too_long = spcs + str_width(h) + str_width(&*spec_vals) >= self.term_w;

        // Is help on next line, if so then indent
        if nlh || self.force_next_line {
            self.none(&format!("\n{}{}{}", TAB, TAB, TAB))?;
        }

        debug!("Help::sc_help: Too long...");
        if too_long && spcs <= self.term_w {
            debug!("Yes");
            debug!("Help::sc_help: help...{}", help);
            debug!("Help::sc_help: help width...{}", str_width(&*help));
            // Determine how many newlines we need to insert
            let avail_chars = self.term_w - spcs;
            debug!("Help::sc_help: Usable space...{}", avail_chars);
            help = wrap_help(&help, avail_chars);
        } else {
            debug!("No");
        }
        if let Some(part) = help.lines().next() {
            self.none(part)?;
        }
        for part in help.lines().skip(1) {
            self.none("\n")?;
            if nlh || self.force_next_line {
                self.none(&format!("{}{}{}", TAB, TAB, TAB))?;
            } else {
                write_nspaces!(self, self.longest + 8);
            }
            self.none(part)?;
        }
        if !help.contains('\n') && (nlh || self.force_next_line) {
            self.none("\n")?;
        }
        Ok(())
    }
}

// Methods to write Parser help.
impl<'b, 'c, 'd, 'w> Help<'b, 'c, 'd, 'w> {
    /// Writes help for all arguments (options, flags, args, subcommands)
    /// including titles of a Parser Object to the wrapped stream.
    pub(crate) fn write_all_args(&mut self) -> ClapResult<()> {
        debug!("Help::write_all_args");
        let flags = self.parser.has_flags();
        // FIXME: Strange filter/count vs fold... https://github.com/rust-lang/rust/issues/33038
        let pos = self.parser.app.get_positionals().fold(0, |acc, arg| {
            if should_show_arg(self.use_long, arg) {
                acc + 1
            } else {
                acc
            }
        }) > 0;
        let opts = self
            .parser
            .app
            .get_opts_no_heading()
            .filter(|arg| should_show_arg(self.use_long, arg))
            .collect::<Vec<_>>();
        let subcmds = self.parser.has_visible_subcommands();

        let custom_headings = self
            .parser
            .app
            .args
            .args
            .iter()
            .filter_map(|arg| arg.help_heading)
            .collect::<IndexSet<_>>();

        let mut first = if pos {
            self.warning("ARGS:\n")?;
            self.write_args_unsorted(&self.parser.app.get_positionals().collect::<Vec<_>>())?;
            false
        } else {
            true
        };

        let unified_help = self.parser.is_set(AppSettings::UnifiedHelpMessage);

        if unified_help && (flags || !opts.is_empty()) {
            let opts_flags = self
                .parser
                .app
                .args
                .args
                .iter()
                .filter(|a| a.has_switch())
                .collect::<Vec<_>>();
            if !first {
                self.none("\n\n")?;
            }
            self.warning("OPTIONS:\n")?;
            self.write_args(&*opts_flags)?;
            first = false;
        } else {
            if flags {
                if !first {
                    self.none("\n\n")?;
                }
                self.warning("FLAGS:\n")?;
                let flags_v: Vec<_> = self.parser.app.get_flags_no_heading().collect();
                self.write_args(&flags_v)?;
                first = false;
            }
            if !opts.is_empty() {
                if !first {
                    self.none("\n\n")?;
                }
                self.warning("OPTIONS:\n")?;
                self.write_args(&opts)?;
                first = false;
            }
            if !custom_headings.is_empty() {
                for heading in custom_headings {
                    if !first {
                        self.none("\n\n")?;
                    }
                    self.warning(&*format!("{}:\n", heading))?;
                    let args = self
                        .parser
                        .app
                        .args
                        .args
                        .iter()
                        .filter(|a| {
                            if let Some(help_heading) = a.help_heading {
                                return help_heading == heading;
                            }
                            false
                        })
                        .collect::<Vec<_>>();
                    self.write_args(&*args)?;
                    first = false
                }
            }
        }

        if subcmds {
            if !first {
                self.none("\n\n")?;
            }
            self.warning("SUBCOMMANDS:\n")?;
            self.write_subcommands(&self.parser.app)?;
        }

        Ok(())
    }

    /// Writes help for subcommands of a Parser Object to the wrapped stream.
    fn write_subcommands(&mut self, app: &App<'b>) -> io::Result<()> {
        debug!("Help::write_subcommands");
        // The shortest an arg can legally be is 2 (i.e. '-x')
        self.longest = 2;
        let mut ord_m = VecMap::new();
        for sc in app
            .subcommands
            .iter()
            .filter(|s| !s.is_set(AppSettings::Hidden))
        {
            let btm = ord_m.entry(sc.disp_ord).or_insert(BTreeMap::new());
            self.longest = cmp::max(self.longest, str_width(sc.name.as_str()));
            btm.insert(sc.name.clone(), sc.clone());
        }

        let mut first = true;
        for btm in ord_m.values() {
            for sc in btm.values() {
                if first {
                    first = false;
                } else {
                    self.none("\n")?;
                }
                self.write_subcommand(sc)?;
            }
        }
        Ok(())
    }

    /// Writes version of a Parser Object to the wrapped stream.
    fn write_version(&mut self) -> io::Result<()> {
        debug!("Help::write_version");
        self.none(self.parser.app.version.unwrap_or(""))?;
        Ok(())
    }

    /// Writes binary name of a Parser Object to the wrapped stream.
    fn write_bin_name(&mut self) -> io::Result<()> {
        debug!("Help::write_bin_name");
        let term_w = self.term_w;
        macro_rules! write_name {
            () => {{
                self.good(&*wrap_help(&self.parser.app.name, term_w))?;
            }};
        }
        if let Some(bn) = self.parser.app.bin_name.as_ref() {
            if bn.contains(' ') {
                // Incase we're dealing with subcommands i.e. git mv is translated to git-mv
                self.good(&bn.replace(" ", "-"))?
            } else {
                write_name!();
            }
        } else {
            write_name!();
        }
        Ok(())
    }

    /// Writes default help for a Parser Object to the wrapped stream.
    pub(crate) fn write_default_help(&mut self) -> ClapResult<()> {
        debug!("Help::write_default_help");
        if let Some(h) = self.parser.app.before_help {
            self.write_before_after_help(h)?;
            self.none("\n\n")?;
        }

        macro_rules! write_thing {
            ($thing:expr) => {{
                self.none(&wrap_help(&$thing, self.term_w))?;
                self.none("\n")?
            }};
        }

        // Print the version
        self.write_bin_name()?;
        self.none(" ")?;
        self.write_version()?;
        self.none("\n")?;

        if let Some(author) = self.parser.app.author {
            write_thing!(author);
        }

        if self.use_long && self.parser.app.long_about.is_some() {
            debug!("Help::write_default_help: writing long about");
            write_thing!(self.parser.app.long_about.unwrap());
        } else if self.parser.app.about.is_some() {
            debug!("Help::write_default_help: writing about");
            write_thing!(self.parser.app.about.unwrap());
        }

        self.none("\n")?;
        self.warning("USAGE:")?;
        self.none(&format!(
            "\n{}{}\n\n",
            TAB,
            Usage::new(self.parser).create_usage_no_title(&[])
        ))?;

        let flags = self.parser.has_flags();
        let pos = self.parser.has_positionals();
        let opts = self.parser.has_opts();
        let subcmds = self.parser.has_subcommands();

        if flags || opts || pos || subcmds {
            self.write_all_args()?;
        }

        if let Some(h) = self.parser.app.after_help {
            if flags || opts || pos || subcmds {
                self.none("\n\n")?;
            }
            self.write_before_after_help(h)?;
        }

        self.writer.finish().map_err(Error::from)
    }
}

// Methods to write Parser help using templates.
impl<'b, 'c, 'd, 'w> Help<'b, 'c, 'd, 'w> {
    /// Write help to stream for the parser in the format defined by the template.
    ///
    /// Tags arg given inside curly brackets:
    /// Valid tags are:
    ///     * `{bin}`         - Binary name.
    ///     * `{version}`     - Version number.
    ///     * `{author}`      - Author information.
    ///     * `{usage}`       - Automatically generated or given usage string.
    ///     * `{all-args}`    - Help for all arguments (options, flags, positionals arguments,
    ///                         and subcommands) including titles.
    ///     * `{unified}`     - Unified help for options and flags.
    ///     * `{flags}`       - Help for flags.
    ///     * `{options}`     - Help for options.
    ///     * `{positionals}` - Help for positionals arguments.
    ///     * `{subcommands}` - Help for subcommands.
    ///     * `{after-help}`  - Info to be displayed after the help message.
    ///     * `{before-help}` - Info to be displayed before the help message.
    ///
    /// The template system is, on purpose, very simple. Therefore the tags have to be written
    /// in the lowercase and without spacing.
    fn write_templated_help(&mut self, template: &str) -> ClapResult<()> {
        debug!("Help::write_templated_help");

        // The strategy is to copy the template from the reader to wrapped stream
        // until a tag is found. Depending on its value, the appropriate content is copied
        // to the wrapped stream.
        // The copy from template is then resumed, repeating this sequence until reading
        // the complete template.

        macro_rules! tags {
            (
                match $part:ident {
                    $( $tag:expr => $action:stmt )*
                }
            ) => {
                match $part {
                    $(
                        part if part.starts_with(concat!($tag, "}")) => {
                            $action
                            let rest = &part[$tag.len()+1..];
                            self.none(rest)?;
                        }
                    )*

                    // Unknown tag, write it back.
                    part => {
                        self.none("{")?;
                        self.none(part)?;
                    }
                }
            };
        }

        let mut parts = template.split('{');
        if let Some(first) = parts.next() {
            self.none(first)?;
        }

        for part in parts {
            self.none(part)?;

            tags! {
                match part {
                    "bin" => {
                        self.write_bin_name()?;
                    }
                    "version" => {
                        self.none(self.parser.app.version.unwrap_or("unknown version"))?;
                    }
                    "author" => {
                        self.none(self.parser.app.author.unwrap_or("unknown author"))?;
                    }
                    "about" => {
                        self.none(self.parser.app.about.unwrap_or("unknown about"))?;
                    }
                    "long-about" => {
                        self.none(self.parser.app.long_about.unwrap_or("unknown about"))?;
                    }
                    "usage" => {
                        self.none(&Usage::new(self.parser).create_usage_no_title(&[]))?;
                    }
                    "all-args" => {
                        self.write_all_args()?;
                    }
                    "unified" => {
                        let opts_flags = self
                            .parser
                            .app
                            .args
                            .args
                            .iter()
                            .filter(|a| a.has_switch())
                            .collect::<Vec<_>>();
                        self.write_args(&*opts_flags)?;
                    }
                    "flags" => {
                        self.write_args(&*flags!(self.parser.app).collect::<Vec<_>>())?;
                    }
                    "options" => {
                        self.write_args(&*opts!(self.parser.app).collect::<Vec<_>>())?;
                    }
                    "positionals" => {
                        self.write_args(&*positionals!(self.parser.app).collect::<Vec<_>>())?;
                    }
                    "subcommands" => {
                        self.write_subcommands(self.parser.app)?;
                    }
                    "after-help" => {
                        self.none(self.parser.app.after_help.unwrap_or("unknown after-help"))?;
                    }
                    "before-help" => {
                        self.none(self.parser.app.before_help.unwrap_or("unknown before-help"))?;
                    }
                }
            }
        }

        Ok(())
    }
}

fn should_show_arg(use_long: bool, arg: &Arg) -> bool {
    debug!("should_show_arg: use_long={:?}, arg={}", use_long, arg.name);
    if arg.is_set(ArgSettings::Hidden) {
        return false;
    }
    (!arg.is_set(ArgSettings::HiddenLongHelp) && use_long)
        || (!arg.is_set(ArgSettings::HiddenShortHelp) && !use_long)
        || arg.is_set(ArgSettings::NextLineHelp)
}

fn wrap_help(help: &str, avail_chars: usize) -> String {
    let wrapper = textwrap::Wrapper::new(avail_chars).break_words(false);
    help.lines()
        .map(|line| wrapper.fill(line))
        .collect::<Vec<String>>()
        .join("\n")
}

#[cfg(test)]
mod test {
    use super::wrap_help;

    #[test]
    fn wrap_help_last_word() {
        let help = String::from("foo bar baz");
        assert_eq!(wrap_help(&help, 5), "foo\nbar\nbaz");
    }
}
