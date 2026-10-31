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
    check_inner(command, cwd, 0, false)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
