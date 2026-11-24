//! `completions <bash|zsh|fish>`: shell completion scripts for the command
//! line, written from one table of its commands and their options. A test
//! checks the table against the parser and the help.
use osm_tile_compiler::OMT_LAYERS as LAYERS;

/// What follows an option, or a command's arguments.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Value {
    /// Nothing: a switch.
    Flag,
    /// A file.
    File,
    /// A directory.
    Dir,
    /// One of these words.
    Words(&'static [&'static str]),
    /// Free text or numbers, which the shell cannot guess.
    Text,
}

use Value::{Dir, File, Flag, Text, Words};

pub(crate) struct Command {
    pub name: &'static str,
    pub about: &'static str,
    /// What the command's positional arguments are.
    pub arguments: Value,
    pub options: &'static [(&'static str, Value)],
}

const FORMATS: &[&str] = &["pmtiles", "mbtiles"];
const TILE_FORMATS: &[&str] = &["mvt", "mlt"];
const TILE_COMPRESSIONS: &[&str] = &["gzip", "none"];
const DATASETS: &[&str] = &["water_polygons", "natural_earth", "lake_centerlines", "all"];
const REPLICATION: &[&str] = &["header", "minute", "hour", "day"];
pub(crate) const SHELLS: &[&str] = &["bash", "zsh", "fish"];

/// The layer names `--layers` and `--exclude-layers` take.
fn layer_words() -> Vec<&'static str> {
    LAYERS.iter().copied().chain(["sar"]).collect()
}

pub(crate) const COMMANDS: &[Command] = &[
    Command {
        name: "compile",
        about: "Compile an OSM PBF into a PMTiles or MBTiles basemap",
        arguments: File,
        options: &[
            ("--request", File),
            ("--area", Text),
            ("--download-dir", Dir),
            ("--output", File),
            ("--format", Words(FORMATS)),
            ("--tile-format", Words(TILE_FORMATS)),
            ("--tile-compression", Words(TILE_COMPRESSIONS)),
            ("--sources", Dir),
            ("--bounds", Text),
            ("--polygon", File),
            ("--area-only", Flag),
            ("--center", Text),
            ("--min-zoom", Text),
            ("--max-zoom", Text),
            ("--threads", Text),
            ("--sort-memory", Text),
            ("--temp-dir", Dir),
            ("--fetch", Flag),
            ("--building-levels", Flag),
            ("--search-index", Flag),
            ("--sar", Flag),
            ("--omit-sources", Words(DATASETS)),
            ("--json", Flag),
            ("--profile", File),
            ("--name", Text),
            ("--description", Text),
            ("--attribution", Text),
            ("--gzip-level", Text),
            ("--schema", File),
            // Filled with the layer names when the script is written.
            ("--layers", Words(&[])),
            ("--exclude-layers", Words(&[])),
            ("--languages", Text),
            ("--changes", File),
            ("--replication", Words(REPLICATION)),
            ("--until", Text),
            ("--max-diffs", Text),
            ("--no-clip", Flag),
            ("--delta-from", File),
            ("--sign-delta", File),
            ("--merge-cache", Flag),
            ("--reuse-from", File),
            ("--resume", Flag),
            ("--data", Text),
            ("--data-layers", File),
        ],
    },
    Command {
        name: "building-levels",
        about: "Write the building levels of a compiled archive",
        arguments: File,
        options: &[("--threads", Text), ("--profile", File)],
    },
    Command {
        name: "search-index",
        about: "Write the search index of a compiled archive",
        arguments: File,
        options: &[("--output", File), ("--threads", Text), ("--json", Flag)],
    },
    Command {
        name: "search",
        about: "Search an archive's index for names",
        arguments: File,
        options: &[
            ("--near", Text),
            ("--within", Text),
            ("--layers", Text),
            ("--limit", Text),
            ("--json", Flag),
        ],
    },
    Command {
        name: "fetch-sources",
        about: "Download or verify the supplemental datasets",
        arguments: Dir,
        options: &[],
    },
    Command {
        name: "schema",
        about: "Verify a tile schema's examples, or list its layers",
        arguments: File,
        options: &[("--json", Flag)],
    },
    Command {
        name: "style",
        about: "Print a MapLibre style, or write an offline style pack",
        arguments: File,
        options: &[
            ("--glyphs", Text),
            ("--sprite", Text),
            ("--offline", Dir),
            ("--archive-file", File),
            ("--languages", Text),
            ("--base-url", Text),
            ("--sar", Flag),
            ("--tile-format", Words(TILE_FORMATS)),
        ],
    },
    Command {
        name: "inspect",
        about: "Describe a PMTiles archive",
        arguments: File,
        options: &[("--json", Flag)],
    },
    Command {
        name: "data-info",
        about: "Describe a vector data file as compile --data reads it",
        arguments: File,
        options: &[("--json", Flag)],
    },
    Command {
        name: "verify",
        about: "Check a PMTiles archive's layout and tiles",
        arguments: File,
        options: &[("--json", Flag)],
    },
    Command {
        name: "extract",
        about: "Cut an area out of a PMTiles archive, local or remote",
        arguments: File,
        options: &[
            ("--output", File),
            ("--bounds", Text),
            ("--polygon", File),
            ("--min-zoom", Text),
            ("--max-zoom", Text),
            ("--json", Flag),
        ],
    },
    Command {
        name: "diff",
        about: "Write a delta between two archives",
        arguments: File,
        options: &[
            ("--output", File),
            ("--sign", File),
            ("--changed-tiles", File),
            ("--json", Flag),
        ],
    },
    Command {
        name: "apply",
        about: "Rebuild an archive from an older one and a delta",
        arguments: File,
        options: &[
            ("--output", File),
            ("--in-place", Flag),
            ("--trusted-key", File),
            ("--json", Flag),
        ],
    },
    Command {
        name: "update",
        about: "Apply OSM changes to a PBF extract",
        arguments: File,
        options: &[
            ("--output", File),
            ("--replication", Words(REPLICATION)),
            ("--until", Text),
            ("--max-diffs", Text),
            ("--bounds", Text),
            ("--no-clip", Flag),
            ("--threads", Text),
            ("--changed-tiles", File),
            ("--min-zoom", Text),
            ("--max-zoom", Text),
            ("--json", Flag),
        ],
    },
    Command {
        name: "serve",
        about: "Serve archives, TileJSON and styles over HTTP",
        arguments: File,
        options: &[("--style-pack", Dir), ("--host", Text), ("--port", Text)],
    },
    Command {
        name: "martin-config",
        about: "Write a Martin tile server config for archives",
        arguments: File,
        options: &[
            ("--output", Dir),
            ("--fonts", Dir),
            ("--sprites", Dir),
            ("--json", Flag),
        ],
    },
    Command {
        name: "keygen",
        about: "Write a new signing key and its public key",
        arguments: File,
        options: &[],
    },
    Command {
        name: "sign",
        about: "Sign a delta in place, or any other file in FILE.sig",
        arguments: File,
        options: &[("--key", File), ("--json", Flag)],
    },
    Command {
        name: "verify-signature",
        about: "Check a delta's or a file's signatures",
        arguments: File,
        options: &[
            ("--trusted-key", File),
            ("--signature", File),
            ("--json", Flag),
        ],
    },
    Command {
        name: "completions",
        about: "Print a shell completion script",
        arguments: Words(SHELLS),
        options: &[],
    },
    Command {
        name: "help",
        about: "Print the help",
        arguments: Flag,
        options: &[],
    },
];

/// The words an option or argument offers, with the layer names filled in.
fn words(name: &str, value: Value) -> Vec<&'static str> {
    match value {
        Words([]) if name.ends_with("layers") => layer_words(),
        Words(words) => words.to_vec(),
        _ => Vec::new(),
    }
}

/// The script for `shell`, or `None` for a shell it does not know.
pub(crate) fn script(shell: &str, program: &str) -> Option<String> {
    match shell {
        "bash" => Some(bash(program)),
        "zsh" => Some(zsh(program)),
        "fish" => Some(fish(program)),
        _ => None,
    }
}

fn bash(program: &str) -> String {
    let function = format!("_{}", program.replace('-', "_"));
    let names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
    let mut out = format!(
        "# bash completion for {program}. Load it with\n\
         #   source <({program} completions bash)\n\
         {function}() {{\n\
         \x20   local cur=\"${{COMP_WORDS[COMP_CWORD]}}\" prev=\"${{COMP_WORDS[COMP_CWORD-1]}}\"\n\
         \x20   local IFS=$'\\n'\n\
         \x20   if [[ $COMP_CWORD -eq 1 ]]; then\n\
         \x20       COMPREPLY=($(IFS=' ' compgen -W \"{} --help --version\" -- \"$cur\"))\n\
         \x20       return\n\
         \x20   fi\n\
         \x20   local opts=\"\" arguments=file\n\
         \x20   case \"${{COMP_WORDS[1]}}\" in\n",
        names.join(" ")
    );
    for command in COMMANDS {
        out.push_str(&format!("        {})\n", command.name));
        let mut cases: Vec<(String, String)> = Vec::new();
        for &(name, value) in command.options {
            let action = match value {
                Flag => continue,
                File => "COMPREPLY=($(compgen -f -- \"$cur\"))".to_owned(),
                Dir => "COMPREPLY=($(compgen -d -- \"$cur\"))".to_owned(),
                Words(_) => format!(
                    "COMPREPLY=($(IFS=' ' compgen -W \"{}\" -- \"$cur\"))",
                    words(name, value).join(" ")
                ),
                Text => "COMPREPLY=()".to_owned(),
            };
            cases.push((name.to_owned(), action));
        }
        if !cases.is_empty() {
            out.push_str("            case \"$prev\" in\n");
            for (name, action) in cases {
                out.push_str(&format!("                {name}) {action}; return ;;\n"));
            }
            out.push_str("            esac\n");
        }
        let options: Vec<&str> = command
            .options
            .iter()
            .map(|(name, _)| *name)
            .chain(["--help"])
            .collect();
        out.push_str(&format!("            opts=\"{}\"\n", options.join(" ")));
        let arguments = match command.arguments {
            File => "file".to_owned(),
            Dir => "dir".to_owned(),
            Words(words) => format!("words:{}", words.join(" ")),
            Flag | Text => "none".to_owned(),
        };
        out.push_str(&format!("            arguments=\"{arguments}\" ;;\n"));
    }
    out.push_str(&format!(
        "    esac\n\
         \x20   if [[ $cur == -* ]]; then\n\
         \x20       COMPREPLY=($(IFS=' ' compgen -W \"$opts\" -- \"$cur\"))\n\
         \x20   else\n\
         \x20       case \"$arguments\" in\n\
         \x20           file) COMPREPLY=($(compgen -f -- \"$cur\")) ;;\n\
         \x20           dir) COMPREPLY=($(compgen -d -- \"$cur\")) ;;\n\
         \x20           words:*) COMPREPLY=($(IFS=' ' compgen -W \"${{arguments#words:}}\" -- \"$cur\")) ;;\n\
         \x20           *) COMPREPLY=() ;;\n\
         \x20       esac\n\
         \x20   fi\n\
         }}\n\
         complete -o filenames -o bashdefault -F {function} {program}\n"
    ));
    out
}

/// Text inside a single-quoted zsh or fish string.
fn quoted(text: &str) -> String {
    text.replace('\'', "'\\''")
}

fn zsh(program: &str) -> String {
    let function = format!("_{}", program.replace('-', "_"));
    let mut out = format!(
        "#compdef {program}\n\
         # zsh completion for {program}. Save it as _{program} in a directory\n\
         # on $fpath, or load it with\n\
         #   source <({program} completions zsh)\n\
         {function}() {{\n\
         \x20   local -a commands\n\
         \x20   commands=(\n"
    );
    for command in COMMANDS {
        out.push_str(&format!(
            "        '{}:{}'\n",
            command.name,
            quoted(command.about)
        ));
    }
    out.push_str(
        "    )\n\
         \x20   if (( CURRENT == 2 )); then\n\
         \x20       _describe -t commands command commands\n\
         \x20       _arguments '--help' '--version'\n\
         \x20       return\n\
         \x20   fi\n\
         \x20   local command=$words[2]\n\
         \x20   shift words\n\
         \x20   (( CURRENT-- ))\n\
         \x20   case $command in\n",
    );
    for command in COMMANDS {
        out.push_str(&format!(
            "        {})\n            _arguments -s",
            command.name
        ));
        for &(name, value) in command.options.iter().chain(&[("--help", Flag)]) {
            let spec = match value {
                Flag => format!("'*{name}'"),
                File => format!("'*{name}:file:_files'"),
                Dir => format!("'*{name}:directory:_files -/'"),
                Words(_) => format!("'*{name}:value:({})'", words(name, value).join(" ")),
                Text => format!("'*{name}:value: '"),
            };
            out.push_str(&format!(" \\\n                {spec}"));
        }
        let arguments = match command.arguments {
            File => Some("'*:file:_files'".to_owned()),
            Dir => Some("'*:directory:_files -/'".to_owned()),
            Words(words) => Some(format!("'*:value:({})'", words.join(" "))),
            Flag | Text => None,
        };
        if let Some(arguments) = arguments {
            out.push_str(&format!(" \\\n                {arguments}"));
        }
        out.push_str(" ;;\n");
    }
    out.push_str(&format!(
        "    esac\n\
         }}\n\
         if [[ $zsh_eval_context[-1] == loadautofunc ]]; then\n\
         \x20   {function} \"$@\"\n\
         else\n\
         \x20   compdef {function} {program}\n\
         fi\n"
    ));
    out
}

fn fish(program: &str) -> String {
    let mut out = format!(
        "# fish completion for {program}. Load it with\n\
         #   {program} completions fish | source\n\
         # or save it as ~/.config/fish/completions/{program}.fish\n\
         complete -c {program} -f\n\
         complete -c {program} -n __fish_use_subcommand -l help -d 'Print the help'\n\
         complete -c {program} -n __fish_use_subcommand -l version -d 'Print the version'\n"
    );
    for command in COMMANDS {
        out.push_str(&format!(
            "complete -c {program} -n __fish_use_subcommand -a {} -d '{}'\n",
            command.name,
            quoted(command.about)
        ));
    }
    for command in COMMANDS {
        let when = format!("-n '__fish_seen_subcommand_from {}'", command.name);
        match command.arguments {
            File => out.push_str(&format!("complete -c {program} {when} -F\n")),
            Dir => out.push_str(&format!(
                "complete -c {program} {when} -a '(__fish_complete_directories)'\n"
            )),
            Words(words) => out.push_str(&format!(
                "complete -c {program} {when} -a '{}'\n",
                words.join(" ")
            )),
            Flag | Text => {}
        }
        for &(name, value) in command.options.iter().chain(&[("--help", Flag)]) {
            let long = name.trim_start_matches("--");
            let rest = match value {
                Flag => String::new(),
                File => " -r -F".to_owned(),
                Dir => " -x -a '(__fish_complete_directories)'".to_owned(),
                Words(_) => format!(" -x -a '{}'", words(name, value).join(" ")),
                Text => " -x".to_owned(),
            };
            out.push_str(&format!("complete -c {program} {when} -l {long}{rest}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::ffi::OsString;

    /// Every option in the table parses, and every option the help names is
    /// in the table.
    #[test]
    fn the_table_matches_the_parser_and_the_help() {
        let mut listed = BTreeSet::new();
        for command in COMMANDS {
            for &(name, value) in command.options {
                listed.insert(name);
                let mut args: Vec<OsString> = vec![command.name.into(), name.into()];
                if value != Flag {
                    let first = words(name, value).first().copied().unwrap_or("1");
                    args.extend([first, "2", "3", "4"].map(OsString::from));
                }
                if let Some(error) = crate::cli::usage_error(&args) {
                    assert!(
                        !error.contains(&format!("option '{name}'")),
                        "{} {name}: {error}",
                        command.name
                    );
                }
            }
            if command.name != "help" {
                let error = crate::cli::usage_error(&[command.name.into(), "--bogus".into()]);
                assert!(
                    error.as_ref().is_some_and(|e| e.contains("unknown")
                        || e.contains("only --threads")
                        || e.contains("completions")),
                    "{}: {error:?}",
                    command.name
                );
            }
        }
        let help = crate::cli::HELP;
        let mut named = BTreeSet::new();
        for (at, _) in help.match_indices("--") {
            let name: String = help[at + 2..]
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                .collect();
            if !name.is_empty() {
                named.insert(format!("--{name}"));
            }
        }
        for global in ["--help", "--version", "--parent-stdin"] {
            named.remove(global);
        }
        let listed: BTreeSet<String> = listed.into_iter().map(String::from).collect();
        assert_eq!(named, listed);
        for command in COMMANDS {
            assert!(
                crate::cli::USAGE.contains(&format!("osm-tile-compiler {}", command.name)),
                "{}",
                command.name
            );
        }
    }

    #[test]
    fn scripts_name_every_command() {
        for shell in SHELLS {
            let script = script(shell, "osm-tile-compiler").unwrap();
            for command in COMMANDS {
                assert!(script.contains(command.name), "{shell}: {}", command.name);
            }
            assert!(script.contains("transportation"), "{shell}");
        }
        assert!(script("tcsh", "osm-tile-compiler").is_none());
    }
}
