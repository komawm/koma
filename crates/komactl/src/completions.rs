//! Dependency-free shell completion generation for `komactl`.
//!
//! The CLI parses arguments by hand, so completions are rendered from a
//! declarative table rather than from `clap`. `table_matches_usage_text` keeps
//! this table and `usage_text()` in sync: a subcommand present in one but not
//! the other fails the test suite.

/// Supported shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    /// Parses a shell name as accepted by `komactl completions <shell>`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }

    /// Canonical shell name used in help text.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
        }
    }
}

/// A single option flag.
pub struct Opt {
    pub long: &'static str,
    pub short: Option<char>,
    pub takes_value: bool,
    pub desc: &'static str,
}

/// A command or subcommand node.
pub struct Cmd {
    pub name: &'static str,
    pub desc: &'static str,
    pub opts: &'static [Opt],
    pub subs: &'static [Cmd],
}

const SANDBOX_RUN_OPTS: &[Opt] = &[
    Opt {
        long: "runtime",
        short: None,
        takes_value: true,
        desc: "Container engine: bwrap or podman",
    },
    Opt {
        long: "ws",
        short: None,
        takes_value: true,
        desc: "Target workspace id, or `auto`",
    },
    Opt {
        long: "name",
        short: None,
        takes_value: true,
        desc: "Workspace name when creating one",
    },
    Opt {
        long: "geometry",
        short: None,
        takes_value: true,
        desc: "Headless viewport, e.g. 1920x1080",
    },
    Opt {
        long: "label",
        short: None,
        takes_value: true,
        desc: "Audit label for the jailed socket",
    },
    Opt {
        long: "image",
        short: None,
        takes_value: true,
        desc: "OCI image for rootless Podman",
    },
    Opt {
        long: "bind",
        short: None,
        takes_value: true,
        desc: "Read-write bind mount host:guest",
    },
    Opt {
        long: "ro-bind",
        short: None,
        takes_value: true,
        desc: "Read-only bind mount host:guest",
    },
    Opt {
        long: "share-net",
        short: None,
        takes_value: false,
        desc: "Enable container network access",
    },
    Opt {
        long: "no-gpu",
        short: None,
        takes_value: false,
        desc: "Omit /dev/dri passthrough",
    },
    Opt {
        long: "nvidia",
        short: None,
        takes_value: false,
        desc: "Add the NVIDIA CDI device",
    },
    Opt {
        long: "with-agent-sock",
        short: None,
        takes_value: false,
        desc: "Mount koma-agent.sock with a scoped token",
    },
    Opt {
        long: "keep-jail",
        short: None,
        takes_value: false,
        desc: "Keep the jail socket after exit",
    },
    Opt {
        long: "dry-run",
        short: None,
        takes_value: false,
        desc: "Print the command without spawning",
    },
];

const SANDBOX_SUBS: &[Cmd] = &[
    Cmd {
        name: "run",
        desc: "Run a GUI command in an isolated Headless workspace",
        opts: SANDBOX_RUN_OPTS,
        subs: &[],
    },
    Cmd {
        name: "create-jail",
        desc: "Create a jailed Wayland socket for a workspace",
        opts: &[],
        subs: &[],
    },
    Cmd {
        name: "revoke-jail",
        desc: "Revoke a jailed Wayland socket",
        opts: &[],
        subs: &[],
    },
];

const ROOT_SUBS: &[Cmd] = &[
    Cmd {
        name: "validate",
        desc: "Dry-run init.lua in a shadow VM",
        opts: &[],
        subs: &[],
    },
    Cmd {
        name: "reload",
        desc: "Atomically hot-reload init.lua",
        opts: &[],
        subs: &[],
    },
    Cmd {
        name: "config-status",
        desc: "Print the active configuration state",
        opts: &[],
        subs: &[],
    },
    Cmd {
        name: "gen-types",
        desc: "Emit LuaLS type stubs",
        opts: &[],
        subs: &[],
    },
    Cmd {
        name: "inventory",
        desc: "Print the workspace and window inventory",
        opts: &[],
        subs: &[],
    },
    Cmd {
        name: "token",
        desc: "Manage capability tokens",
        opts: &[],
        subs: &[Cmd {
            name: "mint",
            desc: "Mint a scoped capability token",
            opts: &[],
            subs: &[],
        }],
    },
    Cmd {
        name: "capture",
        desc: "Capture a workspace frame to a raw file",
        opts: &[],
        subs: &[],
    },
    Cmd {
        name: "sandbox",
        desc: "Run GUI commands in jailed Headless workspaces",
        opts: &[],
        subs: SANDBOX_SUBS,
    },
    Cmd {
        name: "completions",
        desc: "Print a shell completion script",
        opts: &[],
        subs: &[],
    },
];

const ROOT_OPTS: &[Opt] = &[
    Opt {
        long: "help",
        short: Some('h'),
        takes_value: false,
        desc: "Print help",
    },
    Opt {
        long: "version",
        short: Some('V'),
        takes_value: false,
        desc: "Print version",
    },
];

const BIN: &str = "komactl";

static ROOT: Cmd = Cmd {
    name: BIN,
    desc: "",
    opts: &[],
    subs: ROOT_SUBS,
};

/// Renders a completion script for `shell`.
#[must_use]
pub fn render(shell: Shell) -> String {
    match shell {
        Shell::Bash => bash(),
        Shell::Zsh => zsh(),
        Shell::Fish => fish(),
    }
}

fn words_for(node: &Cmd) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for opt in node.opts.iter().chain(ROOT_OPTS.iter()) {
        words.push(format!("--{}", opt.long));
        if let Some(short) = opt.short {
            words.push(format!("-{short}"));
        }
    }
    words.extend(node.subs.iter().map(|sub| sub.name.to_owned()));
    words
}

/// `(path, node)` for every node that has completable content below the root.
fn nested() -> Vec<(String, &'static Cmd)> {
    fn walk(node: &'static Cmd, prefix: String, acc: &mut Vec<(String, &'static Cmd)>) {
        for sub in node.subs {
            let path = format!("{prefix}{} ", sub.name);
            if !sub.subs.is_empty() || !sub.opts.is_empty() {
                acc.push((path.clone(), sub));
            }
            if !sub.subs.is_empty() {
                walk(sub, path, acc);
            }
        }
    }
    let mut acc = Vec::new();
    walk(&ROOT, String::new(), &mut acc);
    acc
}

fn bash() -> String {
    let mut out = String::new();
    out.push_str("# bash completion for komactl — generated by `komactl completions bash`.\n");
    out.push_str("# Install: komactl completions bash | sudo tee /usr/share/bash-completion/completions/komactl >/dev/null\n");
    out.push_str("_komactl() {\n");
    out.push_str("    local cur=\"${COMP_WORDS[COMP_CWORD]}\" path=\"\" i\n");
    out.push_str("    for ((i = 1; i < COMP_CWORD; i++)); do path+=\"${COMP_WORDS[i]} \"; done\n");
    out.push_str("    case \"$path\" in\n");

    out.push_str(&format!(
        "        \"\") COMPREPLY=($(compgen -W \"{}\" -- \"$cur\")) ;;\n",
        words_for(&ROOT).join(" ")
    ));
    for (path, node) in nested() {
        out.push_str(&format!(
            "        \"{path}\") COMPREPLY=($(compgen -W \"{}\" -- \"$cur\")) ;;\n",
            words_for(node).join(" ")
        ));
    }

    out.push_str("        *) COMPREPLY=() ;;\n");
    out.push_str("    esac\n}\n");
    out.push_str("complete -F _komactl komactl\n");
    out
}

fn zsh() -> String {
    let mut out = String::new();
    out.push_str("#compdef komactl\n");
    out.push_str("# zsh completion for komactl — generated by `komactl completions zsh`.\n");
    out.push_str("# Install: komactl completions zsh | sudo tee /usr/share/zsh/site-functions/_komactl >/dev/null\n");
    out.push_str("_komactl() {\n");
    out.push_str("    local path=\"${(j: :)${(@)words[2,CURRENT-1]}}\"\n");
    out.push_str("    case \"$path\" in\n");

    out.push_str(&format!(
        "        \"\") compadd -- {} ;;\n",
        words_for(&ROOT).join(" ")
    ));
    for (path, node) in nested() {
        out.push_str(&format!(
            "        \"{path}\") compadd -- {} ;;\n",
            words_for(node).join(" ")
        ));
    }

    out.push_str("    esac\n}\n");
    out.push_str("compdef _komactl komactl\n");
    out
}

fn fish_condition(parents: &[&str]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for parent in parents {
        parts.push(format!("__fish_seen_subcommand_from {parent}"));
    }
    parts.join("; and ")
}

fn fish_flag_line(condition: &str, opt: &Opt) -> String {
    let mut line = format!("complete -c {BIN}");
    if !condition.is_empty() {
        line.push_str(&format!(" -n '{condition}'"));
    }
    line.push_str(&format!(" -l {} -d '{}'", opt.long, opt.desc));
    if let Some(short) = opt.short {
        line.push_str(&format!(" -s {short}"));
    }
    if opt.takes_value {
        line.push_str(" -r");
    }
    line.push('\n');
    line
}

fn fish() -> String {
    let mut out = String::new();
    out.push_str("# fish completion for komactl — generated by `komactl completions fish`.\n");
    out.push_str("# Install: komactl completions fish > ~/.config/fish/completions/komactl.fish\n");
    out.push_str(&format!("complete -c {BIN} -f\n"));

    for sub in ROOT_SUBS {
        out.push_str(&format!(
            "complete -c {BIN} -n __fish_use_subcommand -a {} -d '{}'\n",
            sub.name, sub.desc
        ));
    }
    for opt in ROOT_OPTS {
        out.push_str(&fish_flag_line("", opt));
    }
    for sub in ROOT_SUBS {
        for opt in sub.opts {
            out.push_str(&fish_flag_line(&fish_condition(&[sub.name]), opt));
        }
        let names: Vec<&str> = sub.subs.iter().map(|c| c.name).collect();
        for child in sub.subs {
            out.push_str(&format!(
                "complete -c {BIN} -n '{}; and not __fish_seen_subcommand_from {}' -a {} -d '{}'\n",
                fish_condition(&[sub.name]),
                names.join(" "),
                child.name,
                child.desc
            ));
            for opt in child.opts {
                out.push_str(&fish_flag_line(
                    &fish_condition(&[sub.name, child.name]),
                    opt,
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shell_renders_bin_and_subcommands() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let script = render(shell);
            assert!(script.contains(BIN), "{shell:?} missing binary name");
            for sub in ROOT_SUBS {
                assert!(script.contains(sub.name), "{shell:?} missing {}", sub.name);
            }
        }
    }

    #[test]
    fn nested_sandbox_commands_are_completed() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let script = render(shell);
            for nested in ["create-jail", "revoke-jail"] {
                assert!(script.contains(nested), "{shell:?} missing {nested}");
            }
        }
    }

    #[test]
    fn table_matches_usage_text() {
        let usage = crate::usage_text();
        for sub in ROOT_SUBS {
            assert!(
                usage.contains(sub.name),
                "completion table lists `{}` but usage_text does not",
                sub.name
            );
        }
        for line in usage
            .lines()
            .filter(|l| l.trim_start().starts_with("komactl "))
        {
            let name = line
                .trim_start()
                .trim_start_matches("komactl ")
                .split_whitespace()
                .next()
                .unwrap_or_default();
            if name.starts_with('-') {
                continue;
            }
            assert!(
                ROOT_SUBS.iter().any(|s| s.name == name),
                "usage_text documents `{name}` but the completion table does not"
            );
        }
    }
}
