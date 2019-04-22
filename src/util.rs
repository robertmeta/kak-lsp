use crate::context::*;
use crate::types::*;
use itertools::Itertools;
use libc;
use lsp_types::request::GotoDefinitionResponse;
use lsp_types::*;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::{stderr, stdout, Write};
use std::os::unix::fs::DirBuilderExt;
use std::time::Duration;
use std::{env, fs, path, process, thread};
use whoami;

pub fn temp_dir() -> path::PathBuf {
    let mut path = env::temp_dir();
    path.push("kak-lsp");
    let old_mask = unsafe { libc::umask(0) };
    // Ignoring possible error during $TMPDIR/kak-lsp creation to have a chance to restore umask.
    let _ = fs::DirBuilder::new()
        .recursive(true)
        .mode(0o1777)
        .create(&path);
    unsafe {
        libc::umask(old_mask);
    }
    path.push(whoami::username());
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&path)
        .unwrap();
    path
}

pub fn lsp_range_to_kakoune(range: Range) -> String {
    // LSP ranges are 0-based, but Kakoune's 1-based.
    // LSP ranges are exclusive, but Kakoune's are inclusive.
    // Also from LSP spec: If you want to specify a range that contains a line including
    // the line ending character(s) then use an end position denoting the start of the next
    // line.
    let start_line = range.start.line;
    let start_char = range.start.character;
    let mut end_line = range.end.line;
    let mut end_char = range.end.character;

    // Some language servers tend to return 0-length ranges.
    if start_line == end_line && start_char == end_char {
        end_char += 1;
    }

    if end_char > 0 {
        end_line += 1;
    } else {
        end_char = 1_000_000;
    }

    format!(
        "{}.{},{}.{}",
        start_line + 1,
        start_char + 1,
        end_line,
        end_char,
    )
}

/// Represent list of symbol information as filetype=grep buffer content.
/// Paths are converted into relative to project root.
pub fn format_symbol_information(items: Vec<SymbolInformation>, ctx: &Context) -> String {
    items
        .into_iter()
        .map(|symbol| {
            let SymbolInformation {
                location,
                name,
                kind,
                ..
            } = symbol;
            let filename = location.uri.to_file_path().unwrap();
            let filename = filename
                .strip_prefix(&ctx.root_path)
                .ok()
                .and_then(|p| p.to_str())
                .or_else(|| filename.to_str())
                .unwrap();

            let position = location.range.start;
            let description = format!("{:?} {}", kind, name);
            format!(
                "{}:{}:{}:{}",
                filename,
                position.line + 1,
                position.character + 1,
                description
            )
        })
        .join("\n")
}

/// Represent list of document symbol as filetype=grep buffer content.
/// Paths are converted into relative to project root.
pub fn format_document_symbol(
    items: Vec<DocumentSymbol>,
    meta: &EditorMeta,
    ctx: &Context,
) -> String {
    items
        .into_iter()
        .map(|symbol| {
            let DocumentSymbol {
                range, name, kind, ..
            } = symbol;
            let filename = path::PathBuf::from(&meta.buffile);
            let filename = filename
                .strip_prefix(&ctx.root_path)
                .ok()
                .and_then(|p| p.to_str())
                .unwrap_or(&meta.buffile);

            let position = range.start;
            let description = format!("{:?} {}", kind, name);
            format!(
                "{}:{}:{}:{}",
                filename,
                position.line + 1,
                position.character + 1,
                description
            )
        })
        .join("\n")
}

/// Escape Kakoune string wrapped into single quote
pub fn editor_escape(s: &str) -> String {
    s.replace("'", "''")
}

/// Convert to Kakoune string by wrapping into quotes and escaping
pub fn editor_quote(s: &str) -> String {
    format!("'{}'", editor_escape(s))
}

// Cleanup and gracefully exit
pub fn goodbye(config: &Config, code: i32) {
    if code == 0 {
        if let Some(ref session) = config.server.session {
            let path = temp_dir();
            let sock_path = path.join(session);
            let pid_path = path.join(format!("{}.pid", session));
            if fs::remove_file(sock_path).is_err() {
                warn!("Failed to remove socket file");
            };
            if pid_path.exists() && fs::remove_file(pid_path).is_err() {
                warn!("Failed to remove pid file");
            };
        }
    }
    stderr().flush().unwrap();
    stdout().flush().unwrap();
    // give stdio a chance to actually flush
    thread::sleep(Duration::from_secs(1));
    process::exit(code);
}

pub fn apply_text_edits(uri: Option<&Url>, text_edits: &[TextEdit]) -> String {
    // empty text edits processed as a special case because Kakoune's `select` command
    // doesn't support empty arguments list
    if text_edits.is_empty() {
        // nothing to do, but sending command back to the editor is required to handle case when
        // editor is blocked waiting for response via fifo
        return "nop".to_string();
    }
    let mut edits = text_edits
        .iter()
        .map(lsp_text_edit_to_kakoune)
        .collect::<Vec<_>>();

    // Adjoin selections detection and Kakoune side editing relies on edits being ordered left to
    // right. Language servers usually send them such, but spec doesn't say anything about the order
    // hence we ensure it by sorting. It's improtant to use stable sort to handle properly cases
    // like multiple inserts in the same place.
    edits.sort_by_key(|x| {
        (
            x.range.start.line,
            x.range.start.character,
            x.range.end.line,
            x.range.end.character,
        )
    });

    let select_edits = edits
        .iter()
        .map(
            |KakouneTextEdit {
                 range: Range { start, end },
                 ..
             }| {
                format!(
                    "{}.{},{}.{}",
                    start.line, start.character, end.line, end.character
                )
            },
        )
        .join(" ");

    let adjoin_selections = edits
        .windows(2)
        .enumerate()
        .filter_map(|(i, pair)| {
            let end = pair[0].range.end;
            let start = pair[1].range.start;
            if (end.line == start.line && end.character + 1 == start.character)
                || (end.line + 1 == start.line
                    && end.character == 1_000_000
                    && start.character == 1)
            {
                Some(i)
            } else {
                None
            }
        })
        .collect::<HashSet<_>>();

    let mut selection_index = 0;
    let apply_edits = edits
        .iter()
        .enumerate()
        .map(
            |(
                i,
                KakouneTextEdit {
                    new_text, command, ..
                },
            )| {
                let command = match command {
                    KakouneTextEditCommand::InsertBefore => "lsp-insert-before-selection",
                    KakouneTextEditCommand::InsertAfter => "lsp-insert-after-selection",
                    KakouneTextEditCommand::Replace => "lsp-replace-selection",
                };
                let command = format!(
                    "exec 'z{}<space>'
                    {} {}",
                    if selection_index > 0 {
                        format!("{})", selection_index)
                    } else {
                        String::new()
                    },
                    command,
                    editor_quote(&new_text)
                );
                // Replacing adjoin selection with empty content effectively removes it and requires one
                // less selection cycle after the next restore to get to the next selection.
                if !(adjoin_selections.contains(&i) && new_text.is_empty()) {
                    selection_index += 1;
                }
                command
            },
        )
        .join("\n");

    let command = format!(
        "select {}
            exec -save-regs '' Z
            {}",
        select_edits, apply_edits
    );
    let command = format!("eval -draft -save-regs '^' {}", editor_quote(&command));
    match uri {
        Some(uri) => {
            let buffile = uri.to_file_path().unwrap();
            format!(
                "lsp-apply-edits-to-file {} {}",
                editor_quote(buffile.to_str().unwrap()),
                editor_quote(&command)
            )
        }
        None => command,
    }
}

enum KakouneTextEditCommand {
    InsertBefore,
    InsertAfter,
    Replace,
}

struct KakouneTextEdit {
    range: Range,
    new_text: String,
    command: KakouneTextEditCommand,
}

fn lsp_text_edit_to_kakoune(text_edit: &TextEdit) -> KakouneTextEdit {
    let TextEdit { range, new_text } = text_edit;
    // LSP ranges are 0-based, but Kakoune's 1-based.
    // LSP ranges are exclusive, but Kakoune's are inclusive.
    // Also from LSP spec: If you want to specify a range that contains a line including
    // the line ending character(s) then use an end position denoting the start of the next
    // line.
    let mut start_line = range.start.line;
    let mut start_char = range.start.character;
    let mut end_line = range.end.line;
    let mut end_char = range.end.character;

    let insert = start_line == end_line && start_char == end_char;
    // Beginning of line is a very special case as we need to produce selection on the line
    // to insert, and then insert before that selection. Selecting end of the previous line
    // and inserting after selection doesn't work well for delete+insert cases like this:
    /*
        [
          {
            "range": {
              "start": {
                "line": 5,
                "character": 0
              },
              "end": {
                "line": 6,
                "character": 0
              }
            },
            "newText": ""
          },
          {
            "range": {
              "start": {
                "line": 6,
                "character": 0
              },
              "end": {
                "line": 6,
                "character": 0
              }
            },
            "newText": "	fmt.Println(\"Hello, world!\")\n"
          }
        ]
    */
    let bol_insert = insert && end_char == 0;

    start_line += 1;
    start_char += 1;

    if end_char > 0 {
        end_line += 1;
        if insert {
            start_char = end_char;
        }
    } else if bol_insert {
        end_line += 1;
        end_char = 1;
    } else {
        end_char = 1_000_000;
    }

    KakouneTextEdit {
        range: Range {
            start: Position {
                line: start_line,
                character: start_char,
            },
            end: Position {
                line: end_line,
                character: end_char,
            },
        },
        new_text: new_text.to_owned(),
        command: if bol_insert {
            KakouneTextEditCommand::InsertBefore
        } else if insert {
            KakouneTextEditCommand::InsertAfter
        } else {
            KakouneTextEditCommand::Replace
        },
    }
}

/// Convert language filetypes configuration into a more lookup-friendly form.
pub fn filetype_to_language_id_map(config: &Config) -> HashMap<String, String> {
    let mut filetypes = HashMap::default();
    for (language_id, language) in &config.language {
        for filetype in &language.filetypes {
            filetypes.insert(filetype.clone(), language_id.clone());
        }
    }
    filetypes
}

/// Convert `GotoDefinitionResponse` into `Option<Location>`.
///
/// If multiple locations are present, returns the first one. Transforms LocationLink into Location
/// by dropping source information.
pub fn goto_definition_response_to_location(
    result: Option<GotoDefinitionResponse>,
) -> Option<Location> {
    match result {
        Some(GotoDefinitionResponse::Scalar(location)) => Some(location),
        Some(GotoDefinitionResponse::Array(mut locations)) => {
            if locations.is_empty() {
                None
            } else {
                Some(locations.remove(0))
            }
        }
        Some(GotoDefinitionResponse::Link(mut locations)) => {
            if locations.is_empty() {
                None
            } else {
                let LocationLink {
                    target_uri,
                    target_range,
                    ..
                } = locations.remove(0);

                Some(Location {
                    uri: target_uri,
                    range: target_range,
                })
            }
        }
        None => None,
    }
}
