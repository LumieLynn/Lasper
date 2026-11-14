use clap::{error::ErrorKind, Arg, ArgAction, ArgGroup, Command};
use std::{env, fmt::Write, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo provides OUT_DIR"));
    let mut command = Command::new("lasper-guest-runtime")
        .about("Guest session runtime for systemd-nspawn containers")
        .disable_help_subcommand(true)
        .args_conflicts_with_subcommands(true)
        .subcommand_negates_reqs(true)
        .group(
            ArgGroup::new("mode")
                .args(["identify", "broker"])
                .required(true),
        )
        .arg(
            Arg::new("identify")
                .long("identify")
                .action(ArgAction::SetTrue)
                .help("Write the binary runtime identity to stdout"),
        )
        .arg(
            Arg::new("broker")
                .long("broker")
                .value_name("DIRECTORY")
                .help("Run the broker in an existing guest-root-owned 0700 directory"),
        )
        .subcommand(
            Command::new("wrap")
                .about("Start a broker, then replace this process with a guest program")
                .arg(
                    Arg::new("command")
                        .value_name("PROGRAM")
                        .last(true)
                        .num_args(1..)
                        .required(true)
                        .help("Absolute guest program path followed by its arguments"),
                ),
        );
    command.build();

    // The freestanding entry uses these names without linking a parser or allocator.
    let mut names = String::new();
    for argument in command.get_arguments() {
        let name = argument.get_id().as_str().to_ascii_uppercase();
        if let Some(long) = argument.get_long() {
            writeln!(
                names,
                "pub const {name}_ARGUMENT: &[u8] = b{:?};",
                format!("--{long}")
            )
            .unwrap();
        }
        if let Some(short) = argument.get_short() {
            writeln!(
                names,
                "pub const {name}_SHORT_ARGUMENT: &[u8] = b{:?};",
                format!("-{short}")
            )
            .unwrap();
        }
    }
    for subcommand in command.get_subcommands() {
        writeln!(
            names,
            "pub const {}_COMMAND: &[u8] = b{:?};",
            subcommand.get_name().to_ascii_uppercase(),
            subcommand.get_name(),
        )
        .unwrap();
    }
    fs::write(output.join("cli_names.rs"), names).unwrap();
    fs::write(output.join("help.txt"), command.render_help().to_string()).unwrap();
    let error = command.error(
        ErrorKind::InvalidValue,
        "invalid runtime command or options",
    );
    fs::write(output.join("error.txt"), error.to_string()).unwrap();

    let mut wrap = command.find_subcommand("wrap").unwrap().clone();
    let wrap_name = format!("{} {}", command.get_name(), wrap.get_name());
    wrap = wrap.bin_name(wrap_name);
    fs::write(output.join("wrap_help.txt"), wrap.render_help().to_string()).unwrap();
    let error = wrap.error(
        ErrorKind::MissingRequiredArgument,
        "expected '--' followed by an absolute guest program and its arguments",
    );
    fs::write(output.join("wrap_error.txt"), error.to_string()).unwrap();
}
