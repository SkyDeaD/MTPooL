//! The node installer, run the way a user runs it: fetched from the registrar and piped into a
//! shell on the node.

use crate::{CommandRun, Node, Result};

/// How the enroll token reaches the installer.
pub enum Token<'a> {
    /// `MTPOOL_TOKEN` in the environment of the installer.
    Env(&'a str),
    /// `--token-file <path>` of a file on the node.
    File(&'a str),
    /// Neither.
    None,
}

pub struct InstallOptions<'a> {
    pub name: &'a str,
    pub token: Token<'a>,
    pub preset: Option<&'a str>,
    pub port: Option<u16>,
}

/// What the installer did.
pub type Installed = CommandRun;

/// A word the shell takes as it is.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

impl Node<'_> {
    /// Runs `curl <registrar>/install.sh | sh -s -- ...` on the node. A failed run is an answer
    /// (see [`CommandRun`]), not an error: a scenario that expects the installer to refuse reads
    /// what it said.
    pub async fn install(&self, options: &InstallOptions<'_>) -> Result<Installed> {
        let mut args = vec!["--name".to_owned(), quote(options.name)];
        if let Token::File(path) = options.token {
            args.push("--token-file".to_owned());
            args.push(quote(path));
        }
        if let Some(preset) = options.preset {
            args.push("--preset".to_owned());
            args.push(quote(preset));
        }
        if let Some(port) = options.port {
            args.push("--port".to_owned());
            args.push(port.to_string());
        }
        // bash for `pipefail`: a registrar that cannot be reached must fail the run, and the
        // installer would otherwise be given an empty script and call that a success.
        let command = format!(
            "bash -o pipefail -c 'curl -fsS http://registrar:8080/install.sh | sh -s -- \"$@\"' \
             install {}",
            args.join(" ")
        );
        let env: Vec<(&str, &str)> = match options.token {
            Token::Env(token) => vec![("MTPOOL_TOKEN", token)],
            _ => Vec::new(),
        };
        self.try_exec(&command, &env).await
    }
}
