use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub rule: &'static str,
    pub reason: String,
}

fn violation(rule: &'static str, reason: &str) -> Option<Violation> {
    Some(Violation {
        rule,
        reason: reason.to_owned(),
    })
}

#[derive(Debug)]
struct Word {
    text: String,
    quoted: bool,
}

fn lex(command: &str) -> Option<Vec<Vec<Word>>> {
    let mut segments = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut active = false;
    let mut quote = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' && matches!(chars.peek(), Some('"' | '\\')) {
                word.push(chars.next()?);
            } else {
                word.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
            quoted = true;
            active = true;
        } else if c.is_whitespace() || matches!(c, ';' | '|' | '&' | '>') {
            if active {
                words.push(Word {
                    text: std::mem::take(&mut word),
                    quoted,
                });
                quoted = false;
                active = false;
            }
            if matches!(c, ';' | '|' | '&') || c == '\n' {
                if !words.is_empty() {
                    segments.push(std::mem::take(&mut words));
                }
            } else if c == '>' {
                words.push(Word {
                    text: ">".into(),
                    quoted: false,
                });
            }
        } else {
            word.push(c);
            active = true;
        }
    }
    if quote.is_some() {
        return None;
    }
    if active {
        words.push(Word { text: word, quoted });
    }
    if !words.is_empty() {
        segments.push(words);
    }
    Some(segments)
}

fn normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn unsafe_target(target: &str, cwd: &Path) -> bool {
    let t = target.trim().replace('\\', "/");
    let lower = t.to_ascii_lowercase();
    if t.is_empty() {
        return false;
    }
    if !cfg!(windows) && t.as_bytes().get(1) == Some(&b':') && t.as_bytes()[0].is_ascii_alphabetic()
    {
        return true;
    }
    if lower == "~"
        || lower.starts_with("~/")
        || lower.contains("$home")
        || lower.contains("%userprofile%")
    {
        return true;
    }
    for name in [
        "userprofile",
        "homepath",
        "home",
        "systemroot",
        "windir",
        "programfiles",
        "localappdata",
        "appdata",
    ] {
        let variable = format!("$env:{name}");
        if lower == variable
            || lower
                .strip_prefix(&variable)
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            return true;
        }
    }
    if lower == ".git"
        || lower.starts_with(".git/")
        || lower.contains("/.git/")
        || lower.ends_with("/.git")
    {
        return true;
    }
    let root = normalized(cwd);
    let has_parent = t.split('/').any(|part| part == "..");
    if Path::new(&t).is_absolute() && normalized(Path::new(&t)) == root {
        return true;
    }
    if lower == "%temp%"
        || lower.starts_with("%temp%/")
        || lower == "$env:temp"
        || lower.starts_with("$env:temp/")
        || lower == "$env:tmp"
        || lower.starts_with("$env:tmp/")
    {
        if has_parent {
            return true;
        }
        let tail = t.split_once('/').map_or("", |(_, rest)| rest);
        return unsafe_target(&std::env::temp_dir().join(tail).to_string_lossy(), cwd);
    }
    if t == "." || t == "*" || t == "./*" || t == ".." || t.starts_with("../") {
        return true;
    }
    let path = Path::new(&t);
    let resolved = if path.is_absolute() {
        normalized(path)
    } else {
        normalized(&root.join(path))
    };
    if resolved.starts_with(&root) {
        if resolved == root {
            return true;
        }
        let relative = resolved.strip_prefix(&root).unwrap_or(&resolved);
        let first = relative
            .components()
            .next()
            .map(|x| x.as_os_str().to_string_lossy())
            .unwrap_or_default();
        return first.contains('*') || first.contains('?') || first.eq_ignore_ascii_case(".git");
    }
    if lower.starts_with("/tmp/")
        || lower.starts_with("/var/tmp/")
        || lower == "/tmp"
        || lower == "/var/tmp"
    {
        return has_parent;
    }
    !resolved.starts_with(normalized(&std::env::temp_dir()))
}

fn inspect(words: &[Word], cwd: &Path, depth: usize, in_cmd: bool) -> Option<Violation> {
    if words.is_empty() {
        return None;
    }
    let mut pos = 0;
    while pos < words.len() {
        let w = words[pos].text.to_ascii_lowercase();
        if matches!(
            w.as_str(),
            "sudo" | "command" | "nohup" | "time" | "env" | "xargs"
        ) || (!words[pos].quoted && w.contains('=') && !w.starts_with('-'))
        {
            pos += 1;
        } else {
            break;
        }
    }
    let words = &words[pos..];
    if words.is_empty() {
        return None;
    }
    let executable = words[0]
        .text
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let exe = executable.strip_suffix(".exe").unwrap_or(&executable);
    let args: Vec<String> = words
        .iter()
        .skip(1)
        .map(|w| w.text.to_ascii_lowercase())
        .collect();
    if depth < 8 && exe == "eval" {
        return words
            .get(1)
            .and_then(|inner| check_inner(&inner.text, cwd, depth + 1, in_cmd));
    }
    if depth < 8 && matches!(exe, "bash" | "sh" | "zsh" | "powershell" | "pwsh" | "cmd") {
        for (i, arg) in args.iter().enumerate() {
            if matches!(arg.as_str(), "-c" | "-command" | "/c") {
                return words
                    .get(i + 2)
                    .and_then(|inner| check_inner(&inner.text, cwd, depth + 1, exe == "cmd"));
            }
        }
    }
    if matches!(
        exe,
        "rm" | "rmdir" | "rd" | "del" | "erase" | "ri" | "remove-item"
    ) {
        let cmd_flags = matches!(exe, "rd" | "del" | "erase")
            || (exe == "rmdir"
                && (in_cmd
                    || (args.iter().any(|a| a == "/s")
                        && !args.iter().any(|a| a.starts_with('-')))));
        let recursive = args.iter().any(|a| {
            a == "--recursive"
                || a == "-recurse"
                || (cmd_flags && a == "/s")
                || (a.starts_with('-') && !a.starts_with("--") && a[1..].chars().any(|c| c == 'r'))
        });
        if recursive
            && words.iter().skip(1).any(|w| {
                !w.text.starts_with('-')
                    && !(cmd_flags && is_cmd_option(&w.text))
                    && unsafe_target(&w.text, cwd)
            })
        {
            return violation("rm-recursive", "recursive removal targets a protected path");
        }
    }
    if exe == "git" {
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            if matches!(
                arg.as_str(),
                "-c" | "--git-dir" | "--work-tree" | "--namespace"
            ) {
                args.get(index + 1)?;
                index += 2;
            } else if matches!(
                arg.as_str(),
                "--no-pager"
                    | "-p"
                    | "--paginate"
                    | "--bare"
                    | "--no-replace-objects"
                    | "--literal-pathspecs"
                    | "--exec-path"
            ) || ["--git-dir=", "--work-tree=", "--namespace=", "--exec-path="]
                .iter()
                .any(|prefix| arg.starts_with(prefix))
            {
                index += 1;
            } else {
                break;
            }
        }
        let cmd = args.get(index).map(String::as_str)?;
        let a = &args[index + 1..];
        let has = |s: &str| a.iter().any(|x| x == s);
        match cmd {
            "reset" if has("--hard") => {
                return violation("git-reset-hard", "git reset discards working tree changes");
            }
            "clean"
                if !has("--dry-run")
                    && !a
                        .iter()
                        .any(|x| x.starts_with('-') && !x.starts_with("--") && x.contains('n'))
                    && a.iter().any(|x| {
                        x == "--force"
                            || x == "--directories"
                            || x.starts_with('-')
                                && !x.starts_with("--")
                                && x[1..].chars().any(|c| "fdx".contains(c))
                    }) =>
            {
                return violation("git-clean", "git clean removes untracked files");
            }
            "push"
                if a.iter().any(|x| {
                    x == "--force"
                        || (x.starts_with('-') && !x.starts_with("--") && x.contains('f'))
                        || x.starts_with('+')
                }) =>
            {
                return violation("git-force-push", "force push can overwrite remote history");
            }
            "checkout" if has("--") || has(".") => {
                return violation("git-discard", "git checkout discards working tree changes");
            }
            "restore" if !has("--staged") && !has("-s") => {
                return violation("git-discard", "git restore discards working tree changes");
            }
            "stash" if a.iter().any(|x| x == "drop" || x == "clear") => {
                return violation("git-discard", "git stash removal discards saved changes");
            }
            "branch"
                if words.iter().skip(index + 2).any(|w| w.text == "-D")
                    || (has("-d") && has("-f")) =>
            {
                return violation("git-discard", "git branch deletion discards a branch");
            }
            "update-ref" if has("-d") => {
                return violation("git-discard", "git update-ref deletes a reference");
            }
            "reflog" if has("expire") => {
                return violation("git-discard", "git reflog expire removes recovery history");
            }
            "gc" if has("--prune=now") => {
                return violation("git-discard", "git gc immediately prunes objects");
            }
            "filter-branch" => {
                return violation("git-discard", "git filter-branch rewrites history");
            }
            _ => {}
        }
    }
    if exe.starts_with("mkfs")
        || exe == "clear-disk"
        || (exe == "diskpart" && args.iter().any(|a| a == "clean"))
        || (exe == "dd" && args.iter().any(|a| a.starts_with("of=/dev/")))
        || (exe == "format"
            && args.iter().any(|a| {
                let a = a.trim_end_matches(['/', '\\']);
                a.len() == 2 && a.ends_with(':')
            }))
        || words
            .windows(2)
            .any(|w| w[0].text == ">" && w[1].text.starts_with("/dev/sd"))
    {
        return violation("disk", "command can overwrite or reformat a disk");
    }
    if matches!(exe, "chmod" | "chown")
        && args.iter().any(|a| a == "-r" || a == "--recursive")
        && args.iter().any(|a| a == "/")
        && (exe == "chown" || args.iter().any(|a| a == "777"))
    {
        return violation(
            "perm",
            "recursive permission changes target the filesystem root",
        );
    }
    if matches!(exe, "psql" | "mysql" | "mariadb" | "sqlite3" | "sqlcmd") {
        for word in words.iter().skip(1) {
            let sql = word.text.to_ascii_lowercase();
            for statement in sql.split(';') {
                let parts: Vec<_> = statement.split_whitespace().collect();
                if matches!(
                    parts.as_slice(),
                    ["drop", "table" | "database" | "schema", ..] | ["truncate", "table", ..]
                ) || (matches!(parts.as_slice(), ["delete", "from", ..])
                    && !parts.contains(&"where"))
                {
                    return violation("sql", "SQL statement destroys data");
                }
            }
        }
    }
    None
}

fn is_cmd_option(word: &str) -> bool {
    let Some(rest) = word.strip_prefix('/') else {
        return false;
    };
    let mut chars = rest.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && matches!(chars.next(), None | Some(':'))
}

fn check_inner(command: &str, cwd: &Path, depth: usize, in_cmd: bool) -> Option<Violation> {
    if depth > 8 {
        return None;
    }
    let mut quote = None;
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\'
            && quote != Some('\'')
            && chars
                .get(i + 1)
                .is_some_and(|next| matches!(next, '$' | '`' | '"' | '\\'))
        {
            i += 2;
            continue;
        }
        if c == '\'' && quote != Some('"') {
            quote = if quote == Some('\'') {
                None
            } else {
                Some('\'')
            };
            i += 1;
            continue;
        }
        if c == '"' && quote != Some('\'') {
            quote = if quote == Some('"') { None } else { Some('"') };
            i += 1;
            continue;
        }
        if quote != Some('\'') && ((c == '$' && chars.get(i + 1) == Some(&'(')) || c == '`') {
            let backtick = c == '`';
            let start = i + if backtick { 1 } else { 2 };
            let mut nesting = 1;
            let mut end = start;
            while end < chars.len() {
                if backtick && chars[end] == '`' {
                    break;
                }
                if !backtick && chars[end] == '(' {
                    nesting += 1;
                }
                if !backtick && chars[end] == ')' {
                    nesting -= 1;
                    if nesting == 0 {
                        break;
                    }
                }
                end += 1;
            }
            if end == chars.len() {
                return None;
            }
            if let Some(v) = check_inner(
                &chars[start..end].iter().collect::<String>(),
                cwd,
                depth + 1,
                in_cmd,
            ) {
                return Some(v);
            }
            i = end + 1;
            continue;
        }
        i += 1;
    }
    lex(command)?
        .iter()
        .find_map(|words| inspect(words, cwd, depth, in_cmd))
}

pub fn check(command: &str, cwd: &Path) -> Option<Violation> {
    check_inner(command, cwd, 0, false).or_else(|| {
        // The service folder holds the passwords of sites: any mention of it
        // is refused, also inside the text of a script.
        if in_department_run(cwd) && without_logs(command).contains(".agentsflow") {
            return Some(foreign_violation());
        }
        // Words that look like paths: a department run must not reach into
        // other departments or the home's own files.
        lex(command)?
            .iter()
            .flatten()
            .filter(|word| word.text.contains(['/', '\\']) || word.text == "..")
            .find_map(|word| foreign_path(&word.text, cwd))
    })
}

fn in_department_run(cwd: &Path) -> bool {
    let cwd = normalized(cwd);
    let named =
        |dir: Option<&Path>, name: &str| dir.and_then(Path::file_name).is_some_and(|n| n == name);
    let runs = cwd.parent();
    let departments = runs.and_then(Path::parent).and_then(Path::parent);
    named(runs, "runs") && named(departments, "departments")
}

fn foreign_violation() -> Violation {
    Violation {
        rule: FOREIGN,
        reason: "папки других отделов и служебные файлы дома закрыты для этого отдела".into(),
    }
}

/// The rule of [`read_only`]: always denied, never asked.
pub const READ_ONLY: &str = "read-only";

/// A read-only agent (a reviewer) may look at everything it is allowed to, but
/// must not change files. `command` is the command of a shell tool. Shell
/// commands are judged by what they look like: a script can still write.
pub fn read_only(tool: &str, command: Option<&str>) -> Option<Violation> {
    let denied = match command {
        Some(command) => writes_files(command, 0),
        None => matches!(
            tool.to_ascii_lowercase().as_str(),
            "write" | "edit" | "multiedit" | "notebookedit" | "task" | "agent"
        ),
    };
    if denied {
        violation(READ_ONLY, "this agent only reads: do not change files")
    } else {
        None
    }
}

fn writes_files(command: &str, depth: usize) -> bool {
    if depth > 3 {
        return true;
    }
    let Some(segments) = lex(command) else {
        return true;
    };
    segments.iter().any(|words| {
        let redirect = words.windows(2).any(|pair| {
            pair[0].text == ">"
                && !pair[0].quoted
                && !matches!(
                    pair[1].text.to_ascii_lowercase().as_str(),
                    "/dev/null" | "nul" | ">"
                )
        });
        let Some(first) = words.iter().find(|word| !word.text.contains('=')) else {
            return redirect;
        };
        let name = first.text.replace('\\', "/");
        let name = name
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        let args = || words.iter().map(|word| word.text.to_ascii_lowercase());
        redirect
            || match name {
                "rm" | "rmdir" | "del" | "mv" | "move" | "cp" | "copy" | "touch" | "mkdir"
                | "md" | "tee" | "ln" | "dd" | "truncate" | "chmod" | "chown" | "install"
                | "set-content" | "add-content" | "out-file" | "new-item" | "remove-item"
                | "move-item" | "copy-item" | "rename-item" => true,
                "sed" | "perl" => args().any(|arg| arg.starts_with("-i")),
                "git" => args()
                    .skip(1)
                    .find(|arg| !arg.starts_with('-'))
                    .is_some_and(|sub| {
                        matches!(
                            sub.as_str(),
                            "add"
                                | "am"
                                | "apply"
                                | "checkout"
                                | "cherry-pick"
                                | "clean"
                                | "commit"
                                | "merge"
                                | "mv"
                                | "pull"
                                | "push"
                                | "rebase"
                                | "reset"
                                | "restore"
                                | "revert"
                                | "rm"
                                | "stash"
                                | "switch"
                        )
                    }),
                "bash" | "sh" | "zsh" | "cmd" | "powershell" | "pwsh" => words
                    .iter()
                    .skip(1)
                    .skip_while(|word| {
                        !matches!(
                            word.text.to_ascii_lowercase().as_str(),
                            "-c" | "/c" | "-command"
                        )
                    })
                    .nth(1)
                    .is_some_and(|inner| writes_files(&inner.text, depth + 1)),
                _ => false,
            }
    })
}

/// The rule of [`foreign_path`]: always denied, never asked.
pub const FOREIGN: &str = "foreign-department";

/// The owner's memory of a project (`~/.claude/projects/<slug>/memory`) is
/// read only for the bot's agents: they keep their own memory next to it.
/// Judged with [`FOREIGN`], so it is refused without asking.
pub fn owner_memory_write(tool: &str, input: &serde_json::Value) -> Option<Violation> {
    let text = |key: &str| input.get(key).and_then(serde_json::Value::as_str);
    let shell = matches!(
        tool.to_ascii_lowercase().as_str(),
        "bash" | "powershell" | "shell"
    );
    let hit = if shell {
        text("command").is_some_and(crate::projectmem::writes_owner_memory)
    } else {
        crate::projectmem::is_write_tool(tool)
            && ["file_path", "path", "notebook_path"]
                .iter()
                .filter_map(|key| text(key))
                .any(|path| crate::projectmem::in_owner_memory(path) || is_log(path))
    };
    hit.then(|| Violation {
        rule: FOREIGN,
        reason: "память хозяина по проекту только для чтения: пиши в память бота, а факт для хозяина — в share.md".into(),
    })
}

/// A run of a department works in `<home>/departments/<id>/runs/<task>`; it
/// may read its own department, but not other departments, the
/// orchestrator or `.agentsflow`. Other working folders are not affected.
pub fn foreign_path(path: &str, cwd: &Path) -> Option<Violation> {
    let cwd = normalized(cwd);
    let runs = cwd.parent()?;
    let department = runs.parent()?;
    let departments = department.parent()?;
    let named = |dir: &Path, name: &str| dir.file_name().is_some_and(|n| n == name);
    if !named(runs, "runs") || !named(departments, "departments") {
        return None;
    }
    let home = departments.parent()?;
    let mut path = path.trim().trim_matches(['"', '\'']).replace('\\', "/");
    // Git Bash on Windows spells C:/x as /c/x or /cygdrive/c/x.
    if cfg!(windows) {
        let msys = path.strip_prefix("/cygdrive").unwrap_or(&path);
        let bytes = msys.as_bytes();
        if bytes.len() >= 2
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && (bytes.len() == 2 || bytes[2] == b'/')
        {
            path = format!("{}:/{}", bytes[1] as char, msys.get(3..).unwrap_or(""));
        }
    }
    let path = Path::new(&path);
    let resolved = if path.is_absolute() {
        normalized(path)
    } else {
        normalized(&cwd.join(path))
    };
    let lower = |p: &Path| p.to_string_lossy().replace('\\', "/").to_lowercase();
    let (resolved, home, department) = (lower(&resolved), lower(home), lower(department));
    let inside = |dir: &str| resolved == dir || resolved.starts_with(&format!("{dir}/"));
    if LOGS
        .iter()
        .any(|log| resolved == format!("{home}/.agentsflow/{log}"))
    {
        return None;
    }
    (inside(&home) && !inside(&department)).then(foreign_violation)
}

/// The gateway's own logs: a department may read them (to find out why the
/// bot misbehaves), nothing else of `.agentsflow`.
pub const LOGS: &[&str] = &["server.log", "gateway.err", "gateway.out", "autostart.log"];

/// The command in lower case with the log paths taken out.
fn without_logs(command: &str) -> String {
    let mut text = command.replace('\\', "/").to_lowercase();
    for log in LOGS {
        text = text.replace(&format!(".agentsflow/{log}"), "");
    }
    text
}

/// Whether a path is one of [`LOGS`].
pub fn is_log(path: &str) -> bool {
    let path = path.replace('\\', "/").to_lowercase();
    LOGS.iter()
        .any(|log| path.ends_with(&format!("/.agentsflow/{log}")))
}

/// Whether a tool call may act outside the run folder: push code, post to
/// GitHub, send a request that changes something, send mail. A silent step
/// that did this is never restarted by itself: the repeat could do it twice.
/// Whatever cannot be classified counts as external.
pub fn external_effect(tool: &str, input: &serde_json::Value) -> bool {
    let name = tool.to_lowercase();
    if matches!(
        name.as_str(),
        "read"
            | "glob"
            | "grep"
            | "ls"
            | "webfetch"
            | "websearch"
            | "todowrite"
            | "file_change"
            | "edit"
            | "write"
            | "multiedit"
            | "notebookedit"
            | "task"
            | "agent"
    ) {
        return false;
    }
    if matches!(
        name.as_str(),
        "bash" | "shell" | "powershell" | "exec_command"
    ) {
        return match input.get("command") {
            Some(serde_json::Value::String(text)) => command_is_external(text, 0),
            // An argv array is judged as it is: joining it into one string
            // would lose which words belong to which argument.
            Some(serde_json::Value::Array(parts)) => {
                let argv: Option<Vec<&str>> = parts.iter().map(|part| part.as_str()).collect();
                argv.is_none_or(|argv| words_are_external(&argv, 0))
            }
            _ => true,
        };
    }
    // Any other tool is a connector: judged by the words in its name. A word
    // that changes something decides; failing that, a word that reads clears
    // it; a name with neither is unknown, and unknown counts as external.
    // `query`, `execute` and `sql` are not reading: a SQL connector changes data
    // through the same tool.
    const CHANGING: [&str; 32] = [
        "send", "post", "create", "delete", "update", "publish", "comment", "push", "merge",
        "write", "upload", "set", "add", "remove", "run", "exec", "execute", "sql", "query",
        "click", "type", "submit", "reply", "forward", "cancel", "trash", "share", "move", "edit",
        "insert", "append", "approve",
    ];
    const READING: [&str; 9] = [
        "get",
        "list",
        "read",
        "search",
        "view",
        "fetch",
        "screenshot",
        "find",
        "status",
    ];
    let words: Vec<&str> = name
        .split(['_', '/', '-', '.', ' '])
        .filter(|word| !word.is_empty())
        .collect();
    if words.iter().any(|word| CHANGING.contains(word)) {
        return true;
    }
    !words.iter().any(|word| READING.contains(word))
}

fn command_is_external(command: &str, depth: usize) -> bool {
    if depth > 3 {
        return true;
    }
    let Some(segments) = lex(command) else {
        return true;
    };
    segments.iter().any(|segment| {
        // A substitution runs something the lexer did not split out.
        if segment.iter().any(|word| {
            ["$(", "`", "<(", ">("]
                .iter()
                .any(|mark| word.text.contains(mark))
        }) {
            return true;
        }
        let words: Vec<&str> = segment.iter().map(|word| word.text.as_str()).collect();
        words_are_external(&words, depth)
    })
}

/// Programs that only read or change files here: a step that ran them has not
/// touched anything outside. Not on the list means unknown, and unknown counts
/// as external.
const LOCAL_PROGRAMS: &[&str] = &[
    "ls",
    "dir",
    "cat",
    "type",
    "head",
    "tail",
    "wc",
    "sort",
    "uniq",
    "cut",
    "tr",
    "tee",
    "echo",
    "printf",
    "pwd",
    "cd",
    "pushd",
    "popd",
    "mkdir",
    "rmdir",
    "cp",
    "mv",
    "rm",
    "touch",
    "ln",
    "stat",
    "file",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "ag",
    "sed",
    "awk",
    "diff",
    "cmp",
    "comm",
    "sha256sum",
    "sha1sum",
    "md5sum",
    "base64",
    "date",
    "whoami",
    "hostname",
    "uname",
    "which",
    "where",
    "chmod",
    "tar",
    "zip",
    "unzip",
    "gzip",
    "gunzip",
    "7z",
    "du",
    "df",
    "tree",
    "less",
    "more",
    "nl",
    "xxd",
    "od",
    "jq",
    "yq",
    "true",
    "false",
    "exit",
    "test",
    "[",
    "sleep",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "rustc",
    "rustfmt",
    "cargo-clippy",
    "pytest",
    "jest",
    "vitest",
    "tsc",
    "eslint",
    "prettier",
    "ruff",
    "black",
    "mypy",
    "gcc",
    "g++",
    "clang",
    "javac",
    // PowerShell
    "get-childitem",
    "gci",
    "get-content",
    "gc",
    "select-string",
    "sls",
    "set-location",
    "sl",
    "get-location",
    "new-item",
    "ni",
    "copy-item",
    "cpi",
    "move-item",
    "mi",
    "remove-item",
    "ri",
    "del",
    "erase",
    "set-content",
    "sc",
    "add-content",
    "ac",
    "out-file",
    "out-null",
    "write-output",
    "write-host",
    "measure-object",
    "where-object",
    "foreach-object",
    "sort-object",
    "select-object",
    "format-list",
    "fl",
    "format-table",
    "ft",
    "convertto-json",
    "convertfrom-json",
    "test-path",
    "resolve-path",
    "split-path",
    "join-path",
    "get-item",
    "get-itemproperty",
    "get-process",
    "get-date",
    "start-sleep",
    "get-command",
    "get-help",
];

/// Shell keywords that only introduce the command that follows.
const SHELL_KEYWORDS: &[&str] = &["if", "then", "else", "elif", "do", "while", "until", "!"];

/// What a Windows or POSIX shell is told to run: the word after its `-c`-like
/// flag, or `None` when it runs a script or something unreadable.
fn shell_inner<'a>(program: &str, rest: &[&'a str]) -> Option<&'a str> {
    let at = rest.iter().position(|word| {
        let lower = word.to_lowercase();
        match program {
            "cmd" => matches!(lower.as_str(), "/c" | "/k"),
            "pwsh" | "powershell" => matches!(lower.as_str(), "-c" | "-command"),
            // `-c`, `-lc`, `-ic`, `-ec`: a cluster of short flags ending in c.
            _ => {
                lower.len() >= 2
                    && lower.starts_with('-')
                    && !lower.starts_with("--")
                    && lower.ends_with('c')
                    && lower[1..].bytes().all(|b| b.is_ascii_lowercase())
            }
        }
    })?;
    rest.get(at + 1).copied()
}

fn words_are_external(words: &[&str], depth: usize) -> bool {
    let mut words: Vec<&str> = words.to_vec();
    // `FOO=bar cmd`, `if x; then cmd`, `timeout 60 cmd` and similar wrappers.
    while let Some(first) = words.first().copied() {
        let assignment = first.split_once('=').is_some_and(|(key, _)| {
            !key.is_empty() && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        });
        if assignment
            || SHELL_KEYWORDS.contains(&first)
            || matches!(
                first,
                "sudo" | "env" | "command" | "nohup" | "time" | "exec"
            )
        {
            words.remove(0);
        } else if first == "timeout" {
            words.remove(0);
            // The duration: `60`, `5m`, `1.5s`.
            if words
                .first()
                .is_some_and(|word| word.starts_with(|c: char| c.is_ascii_digit()))
            {
                words.remove(0);
            }
        } else {
            break;
        }
    }
    // A closing keyword alone does nothing.
    if matches!(words.as_slice(), ["fi"] | ["done"] | ["esac"]) {
        return false;
    }
    let Some(first) = words.first() else {
        return false;
    };
    let program = first
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(first)
        .to_lowercase();
    let program = program.strip_suffix(".exe").unwrap_or(&program).to_owned();
    let rest = &words[1..];
    if program_is_external(&program, rest, depth) {
        return true;
    }
    // Something that runs its arguments (`xargs`, a script block) can hide an
    // outward program anywhere after the first word.
    (1..words.len()).any(|at| {
        let name = words[at]
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(words[at])
            .to_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        matches!(
            name,
            "git" | "gh" | "curl" | "wget" | "ssh" | "scp" | "rsync" | "npm" | "docker"
        ) && program_is_external(name, &words[at + 1..], depth + 1)
    })
}

fn program_is_external(program: &str, rest: &[&str], depth: usize) -> bool {
    let has = |flags: &[&str]| {
        rest.iter().any(|word| {
            flags.iter().any(|flag| {
                word == flag || (flag.starts_with("--") && word.starts_with(&format!("{flag}=")))
            })
        })
    };
    // The method of an HTTP call: `-X POST`, `-XPOST`, `--request POST`, `--request=POST`.
    let method = || -> Option<String> {
        for (at, word) in rest.iter().enumerate() {
            let lower = word.to_lowercase();
            for flag in ["-x", "--request", "--method", "-method"] {
                if lower == flag {
                    return rest.get(at + 1).map(|m| m.to_uppercase());
                }
                if let Some(value) = lower.strip_prefix(&format!("{flag}=")) {
                    return Some(value.to_uppercase());
                }
            }
            // `-XPOST`, also inside a cluster like `-sSXPOST`.
            if word.starts_with('-') && !word.starts_with("--") {
                if let Some(at_x) = word.find('X') {
                    let value = &word[at_x + 1..];
                    return if value.is_empty() {
                        rest.get(at + 1).map(|m| m.to_uppercase())
                    } else {
                        Some(value.to_uppercase())
                    };
                }
            }
        }
        None
    };
    // `-d`, `-F`, `-T` inside a cluster of short flags: `-sd a=1`.
    let short_cluster_with = |letters: &str| {
        rest.iter().any(|word| {
            word.starts_with('-')
                && !word.starts_with("--")
                && word[1..].chars().any(|c| letters.contains(c))
        })
    };
    match program {
        _ if LOCAL_PROGRAMS.contains(&program) => false,
        "find" => has(&["-exec", "-execdir", "-ok", "-okdir"]),
        "go" => !rest
            .iter()
            .find(|word| !word.starts_with('-'))
            .is_none_or(|sub| {
                matches!(
                    *sub,
                    "build" | "test" | "vet" | "fmt" | "mod" | "list" | "env" | "version"
                )
            }),
        "git" => rest
            .iter()
            .any(|word| *word == "push" || *word == "send-email"),
        "cargo" => rest.iter().any(|word| {
            matches!(
                *word,
                "publish" | "login" | "logout" | "owner" | "yank" | "token"
            )
        }),
        "npm" | "pnpm" | "yarn" | "bun" => {
            let sub: Vec<&str> = rest
                .iter()
                .copied()
                .filter(|word| !word.starts_with('-'))
                .collect();
            match sub.as_slice() {
                [] => false,
                ["run" | "run-script", script, ..] => !matches!(
                    *script,
                    "build"
                        | "test"
                        | "lint"
                        | "check"
                        | "format"
                        | "fmt"
                        | "typecheck"
                        | "dev"
                        | "start"
                        | "ci"
                ),
                [
                    "install" | "i" | "ci" | "add" | "test" | "t" | "build" | "ls" | "list"
                    | "outdated" | "audit" | "pack" | "version" | "init" | "lint" | "dev" | "start"
                    | "check" | "format",
                    ..,
                ] => false,
                _ => true,
            }
        }
        "python" | "python3" | "py" => {
            if has(&["--version", "-V"]) {
                return false;
            }
            match rest.iter().position(|word| *word == "-m") {
                Some(at) => !matches!(
                    rest.get(at + 1).copied(),
                    Some(
                        "pytest"
                            | "unittest"
                            | "pip"
                            | "venv"
                            | "black"
                            | "ruff"
                            | "mypy"
                            | "flake8"
                            | "pylint"
                            | "isort"
                            | "compileall"
                    )
                ),
                // A script or `-c` can do anything.
                None => true,
            }
        }
        "pip" | "pip3" => !rest
            .iter()
            .find(|word| !word.starts_with('-'))
            .is_none_or(|sub| {
                matches!(
                    *sub,
                    "install" | "list" | "show" | "freeze" | "uninstall" | "check" | "download"
                )
            }),
        "node" => !has(&["--version", "-v", "--check", "-c", "--test"]),
        "gh" => {
            let sub: Vec<&str> = rest
                .iter()
                .copied()
                .filter(|word| !word.starts_with('-'))
                .take(2)
                .collect();
            match sub.as_slice() {
                [
                    "pr" | "issue" | "run" | "repo" | "release" | "workflow",
                    action,
                ] => !matches!(
                    *action,
                    "view" | "list" | "diff" | "checks" | "status" | "watch" | "download"
                ),
                ["api", ..] => {
                    method().is_some_and(|m| m != "GET")
                        || has(&["-f", "-F", "--field", "--raw-field", "--input"])
                        // `-fkey=value` and `-Fkey=value`
                        || rest.iter().any(|word| {
                            (word.starts_with("-f") || word.starts_with("-F"))
                                && !word.starts_with("--")
                                && word.len() > 2
                        })
                }
                ["auth", "status"] | ["search", _] | ["browse"] => false,
                _ => true,
            }
        }
        "curl" | "http" | "https" | "xh" | "invoke-webrequest" | "iwr" | "invoke-restmethod"
        | "irm" => {
            method().is_some_and(|m| m != "GET" && m != "HEAD")
                || short_cluster_with("dFT")
                || has(&[
                    "--data",
                    "--data-raw",
                    "--data-binary",
                    "--data-urlencode",
                    "--form",
                    "--form-string",
                    "--upload-file",
                    "--json",
                    "-Body",
                    "-InFile",
                ])
        }
        "wget" => {
            method().is_some_and(|m| m != "GET" && m != "HEAD")
                || has(&["--post-data", "--post-file", "--body-data", "--body-file"])
        }
        "bash" | "sh" | "zsh" | "dash" | "pwsh" | "powershell" | "cmd" => {
            match shell_inner(program, rest) {
                Some(inner) => command_is_external(inner, depth + 1),
                // A script, an encoded command, or a flag we do not read.
                None => !rest.is_empty() || program != "cmd",
            }
        }
        // Mail, remote shells and copies, and everything unknown.
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_folder_is_closed_even_inside_a_script() {
        let run = Path::new("/home/x/departments/shop/runs/t1");
        for command in [
            "cat /home/x/.agentsflow/sites.toml",
            "python -c \"print(open('/home/x/.agentsflow/sites.toml').read())\"",
            "cat ../../../../.AGENTSFLOW/sites.toml",
        ] {
            assert_eq!(
                check(command, run).map(|v| v.rule),
                Some(FOREIGN),
                "{command}"
            );
        }
        assert!(check("echo .agentsflow", Path::new("/work/project")).is_none());
    }

    #[test]
    fn command_examples() {
        let cwd = std::env::current_dir()
            .unwrap()
            .join("guard-test-workspace");
        let cases: &[(&str, Option<&str>)] = &[
            (r"rd /s /q build", None),
            (r"del /s /q build", None),
            (r"erase /s /a:h build", None),
            (r"rmdir /s /q .\dist", None),
            (r#"cmd /c "rd /s /q build""#, None),
            (r"rd /s /q C:\", Some("rm-recursive")),
            (r"rd /s /q C:\Windows", Some("rm-recursive")),
            (r"del /s /q C:\*", Some("rm-recursive")),
            (r"rmdir /s /q ..", Some("rm-recursive")),
            (r"rd /s /q .", Some("rm-recursive")),
            ("rm -r /s", Some("rm-recursive")),
            ("rmdir -r /s", Some("rm-recursive")),
            ("git -C repo reset --hard", Some("git-reset-hard")),
            ("git --no-pager reset --hard", Some("git-reset-hard")),
            ("git -c core.x=1 clean -fd", Some("git-clean")),
            ("git --git-dir=.git push --force", Some("git-force-push")),
            ("git -C a -C b branch -D x", Some("git-discard")),
            (
                "git --work-tree repo --git-dir .git reset --hard",
                Some("git-reset-hard"),
            ),
            (
                "git --namespace=test --exec-path=/bin reset --hard",
                Some("git-reset-hard"),
            ),
            ("git -C repo status", None),
            ("git --no-pager log", None),
            ("git -C repo reset --soft HEAD~1", None),
            ("git -C repo push --force-with-lease", None),
            (
                r"Remove-Item -Recurse -Force $env:USERPROFILE",
                Some("rm-recursive"),
            ),
            (
                r"Remove-Item -Recurse -Force $env:USERPROFILE\Documents",
                Some("rm-recursive"),
            ),
            (r"ri -r -fo $HOME", Some("rm-recursive")),
            (r"Remove-Item -Recurse $env:HOMEPATH", Some("rm-recursive")),
            (r"Remove-Item -Recurse $env:HOME", Some("rm-recursive")),
            (
                r"Remove-Item -Recurse $env:SystemRoot",
                Some("rm-recursive"),
            ),
            (r"Remove-Item -Recurse $env:windir", Some("rm-recursive")),
            (
                r"Remove-Item -Recurse $env:ProgramFiles",
                Some("rm-recursive"),
            ),
            (
                r"Remove-Item -Recurse $env:LOCALAPPDATA",
                Some("rm-recursive"),
            ),
            (r"Remove-Item -Recurse $env:APPDATA", Some("rm-recursive")),
            (r"Remove-Item -Recurse -Force $env:TEMP\x", None),
            (r"Remove-Item -Recurse -Force $env:TMP\x", None),
            (r"Remove-Item -Recurse -Force .\target", None),
            (r#"eval "rm -rf /""#, Some("rm-recursive")),
            ("eval 'git reset --hard'", Some("git-reset-hard")),
            ("git push origin main", None),
            ("git checkout main", None),
            ("git branch -d merged", None),
            ("rm -rf node_modules", None),
            ("rm -r -f node_modules", None),
            ("rm --recursive --force dist", None),
            ("cd /tmp && rm -rf cache", None),
            ("dd if=a.img of=b.img", None),
            (r#"psql -c "DELETE FROM users WHERE id=1""#, None),
            ("rm -rf /", Some("rm-recursive")),
            ("rm -r .", Some("rm-recursive")),
            ("rm -R ../outside", Some("rm-recursive")),
            ("rm --recursive .git", Some("rm-recursive")),
            ("rm -rf *", Some("rm-recursive")),
            ("rm -rf ~", Some("rm-recursive")),
            ("rm -rf $HOME", Some("rm-recursive")),
            ("rm -rf %USERPROFILE%", Some("rm-recursive")),
            ("rm -rf target", None),
            ("rm -rf ./build", None),
            ("rm -rf /tmp/x", None),
            ("rm -rf /tmp/../outside", Some("rm-recursive")),
            ("rm -rf %TEMP%/../outside", Some("rm-recursive")),
            ("rm -rf /var/tmp/x", None),
            ("rm file", None),
            ("rmdir /s .", Some("rm-recursive")),
            ("rd /s ..", Some("rm-recursive")),
            ("del /s .git", Some("rm-recursive")),
            ("Remove-Item -Recurse .", Some("rm-recursive")),
            ("Remove-Item -Recurse build", None),
            ("sudo rm -rf /", Some("rm-recursive")),
            ("env X=1 rm -rf /", Some("rm-recursive")),
            ("command rm -rf /", Some("rm-recursive")),
            ("nohup rm -rf /", Some("rm-recursive")),
            ("time rm -rf /", Some("rm-recursive")),
            ("xargs rm -rf /", Some("rm-recursive")),
            ("bash -c 'rm -rf /'", Some("rm-recursive")),
            ("pwsh -Command 'rm -rf /'", Some("rm-recursive")),
            ("cmd /c \"git reset --hard\"", Some("git-reset-hard")),
            ("echo $(git reset --hard)", Some("git-reset-hard")),
            ("echo `git reset --hard`", Some("git-reset-hard")),
            ("grep 'rm -rf /' file", None),
            ("echo 'git reset --hard'", None),
            ("echo '$(git reset --hard)'", None),
            ("echo hi && git reset --hard", Some("git-reset-hard")),
            ("git reset --hard HEAD", Some("git-reset-hard")),
            ("git reset --soft HEAD~1", None),
            ("git clean -fdx", Some("git-clean")),
            ("git clean -n", None),
            ("git clean --force", Some("git-clean")),
            ("git clean -fdn", None),
            ("git push --force", Some("git-force-push")),
            ("git push -f", Some("git-force-push")),
            ("git push origin +main", Some("git-force-push")),
            ("git push --force-with-lease", None),
            ("git checkout -- file", Some("git-discard")),
            ("git checkout .", Some("git-discard")),
            ("git checkout -b feature", None),
            ("git restore file", Some("git-discard")),
            ("git restore --staged file", None),
            ("git stash drop", Some("git-discard")),
            ("git stash clear", Some("git-discard")),
            ("git branch -D x", Some("git-discard")),
            ("git branch -d -f x", Some("git-discard")),
            ("git update-ref -d x", Some("git-discard")),
            ("git reflog expire --all", Some("git-discard")),
            ("git gc --prune=now", Some("git-discard")),
            ("git filter-branch", Some("git-discard")),
            ("mkfs.ext4 /dev/sda", Some("disk")),
            ("dd if=x of=/dev/sda", Some("disk")),
            ("format C:", Some("disk")),
            ("diskpart clean", Some("disk")),
            ("Clear-Disk 1", Some("disk")),
            ("echo hello > /dev/sda", Some("disk")),
            ("psql -c 'DROP TABLE users'", Some("sql")),
            ("mysql -e 'DELETE FROM users'", Some("sql")),
            ("sqlite3 db 'TRUNCATE TABLE x'", Some("sql")),
            ("psql -c 'DELETE FROM users WHERE id=1'", None),
            ("grep 'DROP TABLE' f.sql", None),
            ("chmod -R 777 /", Some("perm")),
            ("chown -R user /", Some("perm")),
            ("chmod -R 777 build", None),
            ("cargo clean", None),
            ("echo hello", None),
            (r"echo \$(git reset --hard)", None),
        ];
        assert!(cases.len() >= 60);
        for (command, expected) in cases {
            assert_eq!(check(command, &cwd).map(|v| v.rule), *expected, "{command}");
        }
        let temp = std::env::temp_dir();
        assert!(unsafe_target("%TEMP%", &temp));
        assert!(unsafe_target(&temp.join("*").to_string_lossy(), &temp));
    }

    #[test]
    fn department_runs_may_only_clean_their_own_folder() {
        // Outside the system temp folder, which the guard leaves open.
        let base = if cfg!(windows) {
            PathBuf::from(r"C:\srv\agentsflow")
        } else {
            PathBuf::from("/srv/agentsflow")
        };
        let run = base.join("departments").join("pr").join("runs").join("t1");
        for (command, expected) in [
            ("rm -rf artifacts/old", None),
            ("rm -rf ./artifacts", None),
            ("rm -rf ../../memory", Some("rm-recursive")),
            ("rm -rf ../t2", Some("rm-recursive")),
            ("rm -rf .", Some("rm-recursive")),
        ] {
            assert_eq!(check(command, &run).map(|v| v.rule), expected, "{command}");
        }
        let home = run.join("../../../..");
        let rules = format!("rm -rf {}", normalized(&home).join("departments").display());
        assert_eq!(check(&rules, &run).map(|v| v.rule), Some("rm-recursive"));
    }

    #[test]
    fn read_only_agents_do_not_change_files() {
        for tool in ["Write", "Edit", "MultiEdit", "NotebookEdit", "Task"] {
            assert_eq!(read_only(tool, None).unwrap().rule, READ_ONLY, "{tool}");
        }
        for tool in ["Read", "Glob", "Grep", "WebSearch"] {
            assert!(read_only(tool, None).is_none(), "{tool}");
        }
        for command in [
            "echo hi > result.md",
            "cat a >> b",
            "rm result.md",
            "/usr/bin/mv a b",
            "tee out.txt",
            "sed -i s/a/b/ f",
            "git commit -m x",
            "ls && touch f",
            "FOO=1 cp a b",
            "bash -c 'echo x > f'",
            "powershell -Command \"Set-Content f x\"",
            "echo 'broken",
        ] {
            assert!(read_only("Bash", Some(command)).is_some(), "{command}");
        }
        for command in [
            "cat result.md",
            "ls -la",
            "grep -n '>' f",
            "git status",
            "git diff HEAD",
            "cargo test 2>&1 | tail",
            "ls 2>/dev/null",
            "sed -n 1,5p f",
            "bash -c 'cat f'",
        ] {
            assert!(read_only("Bash", Some(command)).is_none(), "{command}");
        }
    }

    #[test]
    fn department_runs_stay_out_of_other_departments() {
        let base = if cfg!(windows) {
            PathBuf::from(r"C:\srv\agentsflow")
        } else {
            PathBuf::from("/srv/agentsflow")
        };
        let run = base.join("departments").join("pr").join("runs").join("t1");
        let research = base.join("departments").join("research").join("AGENTS.md");
        for (path, foreign) in [
            ("result.md", false),
            ("../t2/result.md", false),
            ("../../AGENTS.md", false),
            ("../../memory/notes.md", false),
            ("../../../research/AGENTS.md", true),
            ("../../../..", true),
            ("../../../../.agentsflow/board.json", true),
            (research.to_str().unwrap(), true),
            ("C:/Windows/notes.txt", false),
        ] {
            assert_eq!(foreign_path(path, &run).is_some(), foreign, "{path}");
        }
        assert_eq!(
            check("cat ../../../research/memory/notes.md", &run).map(|v| v.rule),
            Some(FOREIGN)
        );
        assert_eq!(check("cat ../../memory/notes.md", &run), None);
        // The gateway's logs are open for reading, the rest of .agentsflow is not.
        let log = base.join(".agentsflow").join("server.log");
        assert!(foreign_path(log.to_str().unwrap(), &run).is_none());
        assert!(foreign_path("../../../../.agentsflow/sites.toml", &run).is_some());
        assert_eq!(check(&format!("tail -50 {}", log.display()), &run), None);
        assert_eq!(
            check("cat ../../../../.agentsflow/sites.toml", &run).map(|v| v.rule),
            Some(FOREIGN)
        );
        assert_eq!(
            check(
                &format!("tail {} ../../../../.agentsflow/board.json", log.display()),
                &run
            )
            .map(|v| v.rule),
            Some(FOREIGN)
        );
        let write = serde_json::json!({"file_path": log.to_str().unwrap()});
        assert!(owner_memory_write("Write", &write).is_some());
        assert!(owner_memory_write("Read", &write).is_none());
        if cfg!(windows) {
            assert_eq!(
                check("cd /c/srv/agentsflow && ls departments", &run).map(|v| v.rule),
                Some(FOREIGN)
            );
            assert_eq!(
                check("cd /c/srv/agentsflow/departments/pr && ls", &run),
                None
            );
        }
        // Outside a department run nothing changes.
        assert_eq!(foreign_path("../../../research/AGENTS.md", &base), None);
    }

    #[test]
    fn external_effects_are_told_from_reading() {
        use serde_json::json;
        let shell = |command: &str| external_effect("Bash", &json!({"command": command}));
        for external in [
            "git push origin agentsflow/t1",
            "cd repo && git push",
            "gh pr create --draft --title x",
            "gh issue comment 4 --body hi",
            "gh api repos/o/r/issues -X POST",
            "gh api repos/o/r/issues -f title=x",
            "curl -X POST https://x.test -d a=1",
            "curl --data-binary @f https://x.test",
            "sendmail a@b.c",
            "bash -c 'git push'",
            "GIT_TERMINAL_PROMPT=0 git push",
            "echo 'unterminated",
            // wrappers, loops and substitutions around the same push
            "bash -lc 'git push origin x'",
            "sh -ec \"git push\"",
            "timeout 60 git push",
            "echo $(git push)",
            "echo `git push`",
            "(git push)",
            "{ git push; }",
            "if true; then git push; fi",
            "xargs git push",
            "powershell.exe -NoProfile -Command 'git push'",
            // methods and bodies in every spelling
            "gh api -XPOST repos/o/r/issues",
            "gh api --method=PATCH repos/o/r/issues/1",
            "gh api repos/o/r/issues -ftitle=x",
            "curl -XPOST https://x.test",
            "curl -sd a=1 https://x.test",
            "curl --request=PUT https://x.test",
            "wget --post-data=a=1 https://x.test",
            // programs that are not known to stay on this machine
            "python -c \"import requests; requests.post('https://x.test')\"",
            "python deploy.py",
            "npm publish",
            "npm run deploy",
            "docker push img",
            "./deploy.sh",
            "ssh host ls",
        ] {
            assert!(shell(external), "{external}");
        }
        for reading in [
            "git status",
            "git commit -m x",
            "gh pr view 3",
            "gh run list --json databaseId",
            "gh api repos/o/r/issues",
            "curl https://x.test",
            "cargo test",
            "ls -la | wc -l",
            "bash -lc 'git status && cargo test'",
            "timeout 60 cargo build",
            "cd repo && grep -rn todo . | head",
            "npm run build",
            "npm install",
            "python -m pytest -q",
            "python --version",
            "find . -name '*.rs'",
            "curl -sSL https://x.test -o f.txt",
            "gh api -XGET repos/o/r/issues",
            "if true; then ls; fi",
        ] {
            assert!(!shell(reading), "{reading}");
        }
        assert!(shell("find . -exec git push \\;"));
        for argv in [
            json!(["git", "push"]),
            json!(["bash", "-lc", "git push"]),
            json!(["powershell.exe", "-Command", "git push origin x"]),
            json!(["sh", "script.sh"]),
        ] {
            assert!(
                external_effect("shell", &json!({ "command": argv })),
                "{argv}"
            );
        }
        for argv in [
            json!(["bash", "-lc", "git status"]),
            json!(["cargo", "test", "--all"]),
            json!(["powershell.exe", "-Command", "Get-ChildItem -Recurse"]),
        ] {
            assert!(
                !external_effect("shell", &json!({ "command": argv })),
                "{argv}"
            );
        }
        assert!(external_effect("Bash", &json!({})));
        assert!(!external_effect("Edit", &json!({"file_path": "a"})));
        assert!(!external_effect("mcp__srv__list_items", &json!({})));
        assert!(external_effect("mcp__srv__send_message", &json!({})));
        assert!(external_effect("mcp__postgres__query", &json!({})));
        assert!(external_effect("mcp__db__execute_query", &json!({})));
        assert!(external_effect("mystery", &json!({})));
    }
}
