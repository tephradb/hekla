//! The hekla language server.
//!
//! Hekla modules are Starlark, but not generic Starlark: the builtins in scope
//! depend on which directory a file sits in, and `load()` resolves against the
//! project root under rules only hekla knows. A generic Starlark server therefore
//! reports every hekla builtin as undefined and mis-resolves every import, which
//! is why hekla serves its own.
//!
//! The protocol plumbing is [`starlark_lsp`]; everything here is the language
//! knowledge it asks for through `LspContext`.

use std::process::ExitCode;

use lsp_server::Connection;
use starlark_lsp::server::{LspServerError, server_with_connection, stdio_server};

use crate::lsp::context::HeklaContext;

pub(crate) mod context;
pub(crate) mod diagnostics;
pub(crate) mod env;
pub(crate) mod project;
pub(crate) mod stubs;

/// Serve the language server over stdio until the client disconnects.
///
/// There is no project directory to pass: each document is placed from its own
/// path, which is what lets one editor session span several hekla projects (this
/// repository's `examples/` holds two).
///
/// Nothing here may write to stdout. Stdout is the protocol, and a stray line
/// would corrupt the frame stream, which is why `hekla lsp` never installs the
/// tracing subscriber `hekla serve` does.
pub fn run(project_checks: bool) -> ExitCode {
    match stdio_server(HeklaContext::new(project_checks)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: language server: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Serve over an already-established connection, for a caller that owns the
/// transport. [`run`] is this over stdio.
///
/// Blocks until the client shuts down, so it wants a thread of its own.
pub fn serve(connection: Connection, project_checks: bool) -> Result<(), LspServerError> {
    server_with_connection(connection, HeklaContext::new(project_checks))
}
