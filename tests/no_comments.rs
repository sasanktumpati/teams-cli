use std::fs;
use std::path::{Path, PathBuf};

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn find_rust_comments(src: &str) -> Vec<(usize, &'static str)> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut line = 1usize;
    let mut hits = Vec::new();

    while i < n {
        let ch = chars[i];
        if ch == '\n' {
            line += 1;
            i += 1;
            continue;
        }

        let raw_prefix = if i == 0 || !is_ident_char(chars[i - 1]) {
            if ch == 'r' {
                1
            } else if (ch == 'b' || ch == 'c') && i + 1 < n && chars[i + 1] == 'r' {
                2
            } else {
                0
            }
        } else {
            0
        };

        if raw_prefix > 0 {
            let mut j = i + raw_prefix;
            let mut hashes = 0usize;
            while j < n && chars[j] == '#' {
                hashes += 1;
                j += 1;
            }
            if j < n && chars[j] == '"' {
                j += 1;
                while j < n {
                    if chars[j] == '\n' {
                        line += 1;
                        j += 1;
                        continue;
                    }
                    if chars[j] == '"' {
                        let mut k = 0;
                        while k < hashes && j + 1 + k < n && chars[j + 1 + k] == '#' {
                            k += 1;
                        }
                        if k == hashes {
                            j += 1 + hashes;
                            break;
                        }
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
        }

        if ch == '"' {
            i += 1;
            while i < n {
                if chars[i] == '\\' {
                    if i + 1 < n && chars[i + 1] == '\n' {
                        line += 1;
                    }
                    i += 2;
                } else if chars[i] == '"' {
                    i += 1;
                    break;
                } else {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            continue;
        }

        if ch == '\'' {
            if i + 1 < n && chars[i + 1] == '\\' {
                let mut j = i + 2;
                while j < n && chars[j] != '\'' && chars[j] != '\n' {
                    j += 1;
                }
                if j < n && chars[j] == '\'' {
                    i = j + 1;
                    continue;
                }
            } else if i + 2 < n
                && chars[i + 1] != '\''
                && chars[i + 1] != '\\'
                && chars[i + 1] != '\n'
                && chars[i + 2] == '\''
            {
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }

        if ch == '/' && i + 1 < n && chars[i + 1] == '/' {
            hits.push((line, "//"));
            i += 2;
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        if ch == '/' && i + 1 < n && chars[i + 1] == '*' {
            hits.push((line, "/*"));
            let mut depth = 1usize;
            i += 2;
            while i < n && depth > 0 {
                if chars[i] == '\n' {
                    line += 1;
                    i += 1;
                } else if chars[i] == '/' && i + 1 < n && chars[i + 1] == '*' {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && i + 1 < n && chars[i + 1] == '/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }

        i += 1;
    }

    hits
}

fn find_ts_comments(src: &str) -> Vec<(usize, &'static str)> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut line = 1usize;
    let mut hits = Vec::new();
    let mut regex_allowed = true;
    let mut template_brace_stack: Vec<usize> = Vec::new();
    let mut brace_depth = 0usize;

    while i < n {
        let ch = chars[i];
        if ch == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if ch.is_whitespace() {
            i += 1;
            continue;
        }

        if ch == '\'' || ch == '"' {
            let quote = ch;
            i += 1;
            while i < n {
                if chars[i] == '\\' {
                    if i + 1 < n && chars[i + 1] == '\n' {
                        line += 1;
                    }
                    i += 2;
                } else if chars[i] == quote {
                    i += 1;
                    break;
                } else {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            regex_allowed = false;
            continue;
        }

        if ch == '`' {
            i += 1;
            while i < n {
                if chars[i] == '\\' {
                    if i + 1 < n && chars[i + 1] == '\n' {
                        line += 1;
                    }
                    i += 2;
                } else if chars[i] == '$' && i + 1 < n && chars[i + 1] == '{' {
                    template_brace_stack.push(brace_depth);
                    brace_depth += 1;
                    regex_allowed = true;
                    i += 2;
                    break;
                } else if chars[i] == '`' {
                    regex_allowed = false;
                    i += 1;
                    break;
                } else {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            continue;
        }

        if ch == '{' {
            brace_depth += 1;
            regex_allowed = true;
            i += 1;
            continue;
        }

        if ch == '}' {
            brace_depth = brace_depth.saturating_sub(1);
            if template_brace_stack.last().copied() == Some(brace_depth) {
                template_brace_stack.pop();
                i += 1;
                while i < n {
                    if chars[i] == '\\' {
                        if i + 1 < n && chars[i + 1] == '\n' {
                            line += 1;
                        }
                        i += 2;
                    } else if chars[i] == '$' && i + 1 < n && chars[i + 1] == '{' {
                        template_brace_stack.push(brace_depth);
                        brace_depth += 1;
                        regex_allowed = true;
                        i += 2;
                        break;
                    } else if chars[i] == '`' {
                        regex_allowed = false;
                        i += 1;
                        break;
                    } else {
                        if chars[i] == '\n' {
                            line += 1;
                        }
                        i += 1;
                    }
                }
                continue;
            }
            regex_allowed = true;
            i += 1;
            continue;
        }

        if ch == '/' && i + 1 < n && chars[i + 1] == '/' {
            hits.push((line, "//"));
            i += 2;
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        if ch == '/' && i + 1 < n && chars[i + 1] == '*' {
            hits.push((line, "/*"));
            i += 2;
            while i < n {
                if chars[i] == '\n' {
                    line += 1;
                    i += 1;
                } else if chars[i] == '*' && i + 1 < n && chars[i + 1] == '/' {
                    i += 2;
                    break;
                } else {
                    i += 1;
                }
            }
            continue;
        }

        if ch == '/' && regex_allowed {
            i += 1;
            let mut in_class = false;
            while i < n && chars[i] != '\n' {
                if chars[i] == '\\' {
                    i += 2;
                } else if chars[i] == '[' {
                    in_class = true;
                    i += 1;
                } else if chars[i] == ']' {
                    in_class = false;
                    i += 1;
                } else if chars[i] == '/' && !in_class {
                    i += 1;
                    while i < n && chars[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                    break;
                } else {
                    i += 1;
                }
            }
            regex_allowed = false;
            continue;
        }

        if ch.is_alphabetic() || ch == '_' || ch == '$' {
            let start = i;
            i += 1;
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            regex_allowed = matches!(
                word.as_str(),
                "return"
                    | "throw"
                    | "case"
                    | "delete"
                    | "void"
                    | "typeof"
                    | "instanceof"
                    | "in"
                    | "of"
                    | "new"
                    | "await"
                    | "yield"
                    | "else"
                    | "do"
            );
            continue;
        }

        if ch.is_ascii_digit() {
            while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_') {
                i += 1;
            }
            regex_allowed = false;
            continue;
        }

        regex_allowed = !matches!(ch, ')' | ']');
        i += 1;
    }

    hits
}

fn find_shell_comments(src: &str) -> Vec<(usize, &'static str)> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut line = 1usize;
    let mut hits = Vec::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut param_depth = 0usize;

    if n >= 2 && chars[0] == '#' && chars[1] == '!' {
        while i < n && chars[i] != '\n' {
            i += 1;
        }
    }

    while i < n {
        let ch = chars[i];
        if ch == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if ch == '\\' {
            if i + 1 < n && chars[i + 1] == '\n' {
                line += 1;
            }
            i += 2;
            continue;
        }
        if in_double {
            if ch == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        if ch == '\'' {
            in_single = true;
            i += 1;
            continue;
        }
        if ch == '"' {
            in_double = true;
            i += 1;
            continue;
        }
        if ch == '$' && i + 1 < n && chars[i + 1] == '{' {
            param_depth += 1;
            i += 2;
            continue;
        }
        if ch == '}' && param_depth > 0 {
            param_depth -= 1;
            i += 1;
            continue;
        }
        if ch == '#' && param_depth == 0 {
            let starts_word = i == 0 || matches!(chars[i - 1], ' ' | '\t' | '\n' | ';' | '|' | '&' | '(' | ')');
            if starts_word {
                hits.push((line, "#"));
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
        }
        i += 1;
    }

    hits
}

fn find_hash_config_comments(src: &str) -> Vec<(usize, &'static str)> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut line = 1usize;
    let mut hits = Vec::new();

    while i < n {
        let ch = chars[i];
        if ch == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            let q = ch;
            let triple = i + 2 < n && chars[i + 1] == q && chars[i + 2] == q;
            i += if triple { 3 } else { 1 };
            while i < n {
                if q == '"' && chars[i] == '\\' {
                    if i + 1 < n && chars[i + 1] == '\n' {
                        line += 1;
                    }
                    i += 2;
                } else if triple && chars[i] == q && i + 2 < n && chars[i + 1] == q && chars[i + 2] == q {
                    i += 3;
                    break;
                } else if !triple && chars[i] == q {
                    i += 1;
                    break;
                } else {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
            continue;
        }
        if ch == '#' {
            hits.push((line, "#"));
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        i += 1;
    }

    hits
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if matches!(name, ".git" | "target" | "node_modules") {
            continue;
        }
        if path.is_dir() {
            collect_files(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

#[test]
fn repository_contains_no_comments() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_files(root, &mut files);
    files.sort();

    let mut violations = Vec::new();
    for path in files {
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");

        let hits = match ext {
            "rs" => {
                let text = fs::read_to_string(&path).unwrap();
                find_rust_comments(&text)
            }
            "ts" | "js" | "mjs" | "cjs" => {
                let text = fs::read_to_string(&path).unwrap();
                find_ts_comments(&text)
            }
            "sh" => {
                let text = fs::read_to_string(&path).unwrap();
                find_shell_comments(&text)
            }
            "toml" | "yml" | "yaml" => {
                let text = fs::read_to_string(&path).unwrap();
                find_hash_config_comments(&text)
            }
            _ if name == ".gitignore" => {
                let text = fs::read_to_string(&path).unwrap();
                find_hash_config_comments(&text)
            }
            _ => continue,
        };

        for (line, kind) in hits {
            violations.push(format!("{}:{}: forbidden `{kind}` comment", rel.display(), line));
        }
    }

    assert!(
        violations.is_empty(),
        "Comments are banned in this repository:\n{}",
        violations.join("\n")
    );
}

#[test]
fn detector_catches_comments_and_ignores_literals() {
    assert_eq!(
        find_rust_comments("fn f() {\n    let x = 1; // hi\n}\n"),
        vec![(2, "//")]
    );
    assert_eq!(find_rust_comments("/// doc\nfn f() {}\n"), vec![(1, "//")]);
    assert_eq!(find_rust_comments("//! mod doc\n"), vec![(1, "//")]);
    assert_eq!(find_rust_comments("fn f() { /* a /* b */ c */ }\n"), vec![(1, "/*")]);
    assert!(find_rust_comments("let s = \"https://example.com/*ok*/\";\n").is_empty());
    assert!(find_rust_comments("let r = r#\"// not a comment /* nope */\"#;\n").is_empty());
    assert!(find_rust_comments("let c = '\"'; let s = \"// ok\"; let q = '/';\n").is_empty());

    assert_eq!(find_ts_comments("const x = 1; // nope\n"), vec![(1, "//")]);
    assert_eq!(find_ts_comments("/** jsdoc */\nconst x = 1;\n"), vec![(1, "/*")]);
    assert!(find_ts_comments("const re = /^\\s*\\d+\\s*$/; const u = `http://127.0.0.1:${p}`;\n").is_empty());

    assert_eq!(
        find_shell_comments("#!/usr/bin/env bash\n# bad\necho ok\n"),
        vec![(2, "#")]
    );
    assert_eq!(find_shell_comments("echo ok # trailing\n"), vec![(1, "#")]);
    assert!(find_shell_comments("#!/usr/bin/env bash\necho \"#ok\" ${#x}\n").is_empty());

    assert_eq!(find_hash_config_comments("key = \"#ok\"\n# bad\n"), vec![(2, "#")]);
    assert!(find_hash_config_comments("color = \"#fff\"\nurl = 'https://a#b'\n").is_empty());
}
