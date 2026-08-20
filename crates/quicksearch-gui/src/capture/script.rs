//! The scenario-script grammar and parser for the capture driver.

use crate::app::Tab;

/// One scenario command. Line-based script, `#` comments outside strings:
///
/// ```text
/// wait_ms INT
/// type "STRING" [cps FLOAT]        # default 7 chars/sec
/// clear_query | focus_search
/// window INT INT                   # resize to width x height, in the same
///                                  # logical points as the startup size
/// hover_match INT                  # pin the pointer over the Nth visible
///                                  # Content Match cell (0-based) until
///                                  # hover_off. Counts every visible row,
///                                  # including those showing a dash.
/// hover_off                        # release the pinned pointer
/// tab (search|manage|duplicates|logs|help|settings)
/// wait_index_running [max INT]     # caps in ms; a capped wait cannot fail
/// wait_index_idle    [max INT]
/// wait_search_done   [max INT]
/// wait_dups_done     [max INT]
/// record_start NAME | record_stop  # NAME: [A-Za-z0-9._-]+, no separators
/// screenshot NAME
/// quit
/// ```
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Cmd {
    WaitMs(u64),
    Type { text: String, cps: f32 },
    ClearQuery,
    FocusSearch,
    Window { w: f32, h: f32 },
    HoverMatch(usize),
    HoverOff,
    Tab(Tab),
    WaitIndexRunning { max_ms: Option<u64> },
    WaitIndexIdle { max_ms: Option<u64> },
    WaitSearchDone { max_ms: Option<u64> },
    WaitDupsDone { max_ms: Option<u64> },
    RecordStart(String),
    RecordStop,
    Screenshot(String),
    Quit,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ParseError {
    /// 1-based line in the scenario file.
    pub line: usize,
    pub msg: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Str(String),
}

/// Split one line into bare words and quoted strings. `#` starts a comment
/// except inside a string; `\"` and `\\` are the only escapes.
fn tokenize(line: &str, line_no: usize) -> Result<Vec<Token>, ParseError> {
    let err = |msg: String| ParseError { line: line_no, msg };
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '#' {
            break;
        } else if c == '"' {
            chars.next();
            let mut s = String::new();
            loop {
                match chars.next() {
                    None => return Err(err("unclosed string".to_string())),
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some(e @ ('"' | '\\')) => s.push(e),
                        Some(e) => return Err(err(format!("unknown escape \\{e}"))),
                        None => return Err(err("unclosed string".to_string())),
                    },
                    Some(other) => s.push(other),
                }
            }
            tokens.push(Token::Str(s));
        } else {
            let mut w = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == '#' {
                    break;
                }
                if c == '"' {
                    return Err(err("quotes may only start a token".to_string()));
                }
                w.push(c);
                chars.next();
            }
            tokens.push(Token::Word(w));
        }
    }
    Ok(tokens)
}

pub(super) fn parse_script(src: &str) -> Result<Vec<Cmd>, ParseError> {
    let mut cmds = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let line_no = i + 1;
        let tokens = tokenize(line, line_no)?;
        if let Some(cmd) = parse_line(&tokens, line_no)? {
            cmds.push(cmd);
        }
    }
    Ok(cmds)
}

fn parse_line(tokens: &[Token], line_no: usize) -> Result<Option<Cmd>, ParseError> {
    let err = |msg: String| ParseError { line: line_no, msg };
    let Some(first) = tokens.first() else {
        return Ok(None);
    };
    let Token::Word(name) = first else {
        return Err(err(
            "a line must start with a command, not a string".to_string()
        ));
    };

    let rest = &mut tokens[1..].iter();
    let cmd = match name.as_str() {
        "wait_ms" => Cmd::WaitMs(parse_int(
            "duration",
            next_word(rest, line_no, "duration in ms")?,
            line_no,
        )?),
        "type" => {
            let text = match rest.next() {
                Some(Token::Str(s)) => s.clone(),
                Some(Token::Word(_)) => {
                    return Err(err("the text to type must be quoted".to_string()));
                }
                None => return Err(err("missing text to type".to_string())),
            };
            let cps = match rest.next() {
                None => 7.0,
                Some(Token::Word(w)) if w == "cps" => {
                    let v = next_word(rest, line_no, "cps value")?;
                    let cps: f32 = v
                        .parse()
                        .map_err(|_| err(format!("invalid cps {v:?}: expected a number")))?;
                    if !(cps.is_finite() && cps > 0.0) {
                        return Err(err("cps must be positive".to_string()));
                    }
                    cps
                }
                Some(other) => return Err(err(format!("expected `cps`, found {other:?}"))),
            };
            Cmd::Type { text, cps }
        }
        "clear_query" => Cmd::ClearQuery,
        "focus_search" => Cmd::FocusSearch,
        "window" => {
            let w = parse_int(
                "width",
                next_word(rest, line_no, "width in points")?,
                line_no,
            )?;
            let h = parse_int(
                "height",
                next_word(rest, line_no, "height in points")?,
                line_no,
            )?;
            if w == 0 || h == 0 {
                return Err(err("window dimensions must be positive".to_string()));
            }
            Cmd::Window {
                w: w as f32,
                h: h as f32,
            }
        }
        "hover_match" => {
            Cmd::HoverMatch(
                parse_int("row", next_word(rest, line_no, "row index")?, line_no)? as usize,
            )
        }
        "hover_off" => Cmd::HoverOff,
        "tab" => Cmd::Tab(match next_word(rest, line_no, "tab name")? {
            "search" => Tab::Search,
            "manage" => Tab::Manage,
            "duplicates" => Tab::Duplicates,
            "logs" => Tab::Logs,
            "help" => Tab::Help,
            "settings" => Tab::Settings,
            other => {
                return Err(err(format!(
                    "unknown tab {other:?}: expected search, manage, duplicates, \
                     logs, help or settings"
                )));
            }
        }),
        "wait_index_running" | "wait_index_idle" | "wait_search_done" | "wait_dups_done" => {
            let max_ms = match rest.next() {
                None => None,
                Some(Token::Word(w)) if w == "max" => Some(parse_int(
                    "max",
                    next_word(rest, line_no, "max value in ms")?,
                    line_no,
                )?),
                Some(other) => return Err(err(format!("expected `max`, found {other:?}"))),
            };
            match name.as_str() {
                "wait_index_running" => Cmd::WaitIndexRunning { max_ms },
                "wait_index_idle" => Cmd::WaitIndexIdle { max_ms },
                "wait_search_done" => Cmd::WaitSearchDone { max_ms },
                _ => Cmd::WaitDupsDone { max_ms },
            }
        }
        "record_start" => Cmd::RecordStart(parse_name(
            next_word(rest, line_no, "output name")?,
            line_no,
        )?),
        "record_stop" => Cmd::RecordStop,
        "screenshot" => Cmd::Screenshot(parse_name(
            next_word(rest, line_no, "output name")?,
            line_no,
        )?),
        "quit" => Cmd::Quit,
        other => return Err(err(format!("unknown command {other:?}"))),
    };

    if let Some(extra) = rest.next() {
        return Err(err(format!(
            "unexpected {extra:?} after a complete command"
        )));
    }
    Ok(Some(cmd))
}

fn next_word<'a>(
    rest: &mut std::slice::Iter<'a, Token>,
    line_no: usize,
    what: &str,
) -> Result<&'a str, ParseError> {
    match rest.next() {
        Some(Token::Word(w)) => Ok(w.as_str()),
        Some(Token::Str(_)) => Err(ParseError {
            line: line_no,
            msg: format!("expected {what}, found a string"),
        }),
        None => Err(ParseError {
            line: line_no,
            msg: format!("missing {what}"),
        }),
    }
}

fn parse_int(what: &str, w: &str, line_no: usize) -> Result<u64, ParseError> {
    w.parse::<u64>().map_err(|_| ParseError {
        line: line_no,
        msg: format!("invalid {what} {w:?}: expected an integer"),
    })
}

/// Output names stay inside `$QS_CAPTURE_OUT`: a plain filename stem, the
/// driver appends the extension.
fn parse_name(w: &str, line_no: usize) -> Result<String, ParseError> {
    let ok = !w.is_empty()
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(w.to_string())
    } else {
        Err(ParseError {
            line: line_no,
            msg: format!("invalid name {w:?}: use only letters, digits, `.`, `_`, `-`"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(line: &str) -> Cmd {
        let cmds = parse_script(line).expect("line should parse");
        assert_eq!(cmds.len(), 1, "expected exactly one command from {line:?}");
        cmds.into_iter().next().unwrap()
    }

    fn parse_err(src: &str) -> ParseError {
        parse_script(src).expect_err("script should be rejected")
    }

    #[test]
    fn every_command_parses() {
        let script = r#"
            wait_ms 250
            type "hello world"
            type "fast" cps 30
            clear_query
            focus_search
            window 500 350
            hover_match 2
            hover_off
            tab search
            tab manage
            tab duplicates
            tab logs
            tab help
            tab settings
            wait_index_running
            wait_index_running max 15000
            wait_index_idle max 13000
            wait_search_done max 6000
            wait_dups_done
            record_start manage-indexing
            record_stop
            screenshot query-highlight.v2
            quit
        "#;
        let cmds = parse_script(script).expect("script should parse");
        assert_eq!(
            cmds,
            vec![
                Cmd::WaitMs(250),
                Cmd::Type {
                    text: "hello world".to_string(),
                    cps: 7.0
                },
                Cmd::Type {
                    text: "fast".to_string(),
                    cps: 30.0
                },
                Cmd::ClearQuery,
                Cmd::FocusSearch,
                Cmd::Window { w: 500.0, h: 350.0 },
                Cmd::HoverMatch(2),
                Cmd::HoverOff,
                Cmd::Tab(Tab::Search),
                Cmd::Tab(Tab::Manage),
                Cmd::Tab(Tab::Duplicates),
                Cmd::Tab(Tab::Logs),
                Cmd::Tab(Tab::Help),
                Cmd::Tab(Tab::Settings),
                Cmd::WaitIndexRunning { max_ms: None },
                Cmd::WaitIndexRunning {
                    max_ms: Some(15000)
                },
                Cmd::WaitIndexIdle {
                    max_ms: Some(13000)
                },
                Cmd::WaitSearchDone { max_ms: Some(6000) },
                Cmd::WaitDupsDone { max_ms: None },
                Cmd::RecordStart("manage-indexing".to_string()),
                Cmd::RecordStop,
                Cmd::Screenshot("query-highlight.v2".to_string()),
                Cmd::Quit,
            ]
        );
    }

    #[test]
    fn string_escapes_and_hash_inside_strings() {
        assert_eq!(
            parse_one(r#"type "say \"hi\" \\ done""#),
            Cmd::Type {
                text: r#"say "hi" \ done"#.to_string(),
                cps: 7.0
            }
        );
        // `#` inside a quoted string is content, not a comment.
        assert_eq!(
            parse_one(r##"type "a # b""##),
            Cmd::Type {
                text: "a # b".to_string(),
                cps: 7.0
            }
        );
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let cmds =
            parse_script("\n# a full-line comment\n   \nquit # trailing comment\n#another\n")
                .expect("should parse");
        assert_eq!(cmds, vec![Cmd::Quit]);
    }

    #[test]
    fn errors_carry_the_right_line_number() {
        let e = parse_err("wait_ms 100\nquit\nfrobnicate\n");
        assert_eq!(e.line, 3);
        assert!(e.msg.contains("frobnicate"), "got: {}", e.msg);
    }

    #[test]
    fn unclosed_string_is_rejected() {
        let e = parse_err("type \"never closed\n");
        assert_eq!(e.line, 1);
        assert!(e.msg.contains("unclosed"), "got: {}", e.msg);
    }

    #[test]
    fn unknown_escape_is_rejected() {
        let e = parse_err(r#"type "a\nb""#);
        assert!(e.msg.contains("escape"), "got: {}", e.msg);
    }

    #[test]
    fn non_numeric_int_is_rejected() {
        let e = parse_err("wait_ms soon");
        assert!(e.msg.contains("integer"), "got: {}", e.msg);
        let e = parse_err("wait_index_idle max never");
        assert!(e.msg.contains("integer"), "got: {}", e.msg);
    }

    #[test]
    fn names_with_path_separators_are_rejected() {
        for bad in [
            "screenshot ../escape",
            "screenshot a/b",
            "record_start a\\b",
        ] {
            let e = parse_err(bad);
            assert!(e.msg.contains("invalid name"), "{bad:?} got: {}", e.msg);
        }
    }

    #[test]
    fn missing_arguments_are_rejected() {
        assert!(parse_err("wait_ms").msg.contains("missing"));
        assert!(parse_err("type").msg.contains("missing"));
        assert!(parse_err("tab").msg.contains("missing"));
        assert!(parse_err("screenshot").msg.contains("missing"));
        assert!(parse_err("window").msg.contains("missing"));
        assert!(parse_err("window 500").msg.contains("missing"));
        assert!(parse_err("hover_match").msg.contains("missing"));
    }

    #[test]
    fn degenerate_window_sizes_are_rejected() {
        assert!(parse_err("window 0 350").msg.contains("positive"));
        assert!(parse_err("window 500 0").msg.contains("positive"));
        assert!(parse_err("window 500 -1").msg.contains("integer"));
    }

    #[test]
    fn trailing_garbage_is_rejected() {
        let e = parse_err("quit now");
        assert!(e.msg.contains("unexpected"), "got: {}", e.msg);
        let e = parse_err("wait_ms 100 200");
        assert!(e.msg.contains("unexpected"), "got: {}", e.msg);
    }

    #[test]
    fn unknown_tab_and_bad_cps_are_rejected() {
        assert!(parse_err("tab preferences").msg.contains("unknown tab"));
        assert!(parse_err(r#"type "x" cps 0"#).msg.contains("positive"));
        assert!(parse_err(r#"type "x" cps -3"#).msg.contains("positive"));
    }

    #[test]
    fn unquoted_type_text_is_rejected() {
        let e = parse_err("type hello");
        assert!(e.msg.contains("quoted"), "got: {}", e.msg);
    }

    /// The scenario that ships in packaging/ must always parse — this pins
    /// the file to the grammar so neither can drift without failing tests.
    #[test]
    fn the_shipped_scenario_parses() {
        let src = include_str!("../../../../packaging/capture-scenario.txt");
        let cmds = parse_script(src).expect("packaging/capture-scenario.txt should parse");
        assert!(
            cmds.len() > 10,
            "scenario looks truncated: {} commands",
            cmds.len()
        );
        assert_eq!(
            cmds.last(),
            Some(&Cmd::Quit),
            "scenario should end with quit"
        );
    }
}
