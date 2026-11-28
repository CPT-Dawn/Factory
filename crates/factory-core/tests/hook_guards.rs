//! The pre-tool guards that keep the shared host whole: CPU burners, commands that stop on an
//! approval dialog nobody answers, REPL wait loops, and JS objects run as commands. Each row is a
//! tool call; every deny has near-misses beside it that must pass, since a false positive stops
//! every agent on the machine.

use factory_core::hook::{PreTool, pre_tool};
use serde_json::{Value, json};

fn bash(command: &str) -> Value {
    json!({"tool_name": "Bash", "tool_input": {"command": command}})
}

fn repl(code: &str) -> Value {
    json!({"tool_name": "REPL", "tool_input": {"code": code}})
}

fn denied(call: &Value) -> Option<String> {
    match pre_tool(call, false) {
        PreTool::Block(reason) => Some(reason),
        _ => None,
    }
}

/// The refusal for each call, which must be one line of at most 200 characters naming `fix`.
fn assert_denied_with(calls: &[Value], fix: &str) {
    for call in calls {
        let reason = denied(call).unwrap_or_else(|| panic!("{call} should be denied"));
        assert!(reason.contains(fix), "{call}: {reason}");
        assert!(!reason.contains('\n'), "{reason}");
        assert!(
            reason.chars().count() <= 200,
            "{} chars: {reason}",
            reason.len()
        );
    }
}

fn assert_passes(calls: &[Value]) {
    for call in calls {
        assert!(
            !matches!(pre_tool(call, false), PreTool::Block(_)),
            "{call} should pass: {:?}",
            pre_tool(call, false)
        );
    }
}

const RM_FIX: &str = "rm on a literal absolute path, as a command of its own";

#[test]
fn an_rm_that_stops_on_an_approval_dialog_is_denied() {
    let commands = [
        "rm $FILE",
        "rm -rf \"$DIR\"",
        "rm -f \"${TMP}/x\"",
        "rm -rf /tmp/build/*",
        "rm *.log",
        "rm file?.txt",
        "rm -f out[12].txt",
        "rm $(cat list.txt)",
        "rm `ls *.tmp`",
        "cd /tmp && rm -rf $X",
        "rm -- $F",
        "sudo rm -rf $D",
        "bash -c 'rm /tmp/x'",
        "sh -c \"rm -f /tmp/a.txt\"",
        "bash -lc 'cd /tmp && rm -rf y'",
    ];
    let calls: Vec<Value> = commands.iter().map(|c| bash(c)).collect();
    assert_denied_with(&calls, RM_FIX);
}

#[test]
fn an_rm_on_literal_paths_passes() {
    let commands = [
        "rm /abs/literal",
        "rm -f ./a.txt",
        "rm -rf /tmp/x/y",
        "rm -f /tmp/a.txt /tmp/b.txt",
        "rm -rf ~/scratch/t12",
        "rm 'my file.txt'",
        "rm -- -weird-name",
        "git rm -r --cached $DIR",
        "rmdir /tmp/x",
        "echo rm $X",
        "bash -c 'ls $HOME'",
        "git commit -m 'stop rm $X'",
        "find /tmp/x -name '*.o' -delete",
    ];
    let calls: Vec<Value> = commands.iter().map(|c| bash(c)).collect();
    assert_passes(&calls);
}

const ANCHOR_FIX: &str = "rg -F";

#[test]
fn a_dollar_inside_a_quoted_grep_pattern_is_denied() {
    let commands = [
        "rg 'foo$'",
        "rg \"foo$\" src",
        "rg -n '^fn main$' crates",
        "rg -i \"^bar$ baz\"",
        "grep -E 'end$' notes.txt",
        "grep -rn \"x$\" src",
        "egrep 'a$|b' f",
        "git grep 'x$'",
        "git -C /tmp/repo grep -n 'y$'",
        "rg -e 'foo$'",
        "rg --regexp='foo$'",
        "grep 'a$' \"$LOG\"",
        "cd src && rg 'a$'",
        "rg -A 3 'end$' src",
        "bash -c \"rg 'x$' src\"",
    ];
    let calls: Vec<Value> = commands.iter().map(|c| bash(c)).collect();
    assert_denied_with(&calls, ANCHOR_FIX);
    let reason = denied(&bash("rg 'foo$'")).unwrap();
    assert!(reason.contains("rg -e"), "{reason}");
}

#[test]
fn an_unquoted_or_fixed_string_dollar_passes() {
    let commands = [
        "rg foo$",
        "rg -e foo$ src",
        "rg -F 'a$b'",
        "rg --fixed-strings 'a$'",
        "grep -F 'x$' f",
        "grep -rnF 'x$' .",
        "fgrep 'a$' f",
        "git grep -F 'x$'",
        "rg 'foo'",
        "rg \"$PATTERN\" src",
        "grep -n \"^${NAME}=\" f",
        "grep x \"$1\"",
        "rg 'a' \"$DIR\"",
        "rg '\\$HOME' src",
        "rg \"\\$HOME\" src",
        "echo 'foo$'",
        "git log --grep='x$'",
        "sed 's/a$/b/' f",
        "awk '{print $1}' f",
        "rg foo -g '!*.lock'",
    ];
    let calls: Vec<Value> = commands.iter().map(|c| bash(c)).collect();
    assert_passes(&calls);
}

#[test]
fn a_js_object_run_as_a_command_is_denied() {
    let commands = [
        "[object Object]",
        "[object Object] > out.txt",
        "cd /tmp && [object Object]",
        "[object Promise]",
    ];
    let calls: Vec<Value> = commands.iter().map(|c| bash(c)).collect();
    assert_denied_with(&calls, "pass a string or array");
}

#[test]
fn a_mention_of_an_object_or_a_test_bracket_passes() {
    let commands = [
        "echo '[object Object]'",
        "rg -F '[object' src",
        "[ -f /tmp/x ] && echo y",
        "[[ -n x ]]",
        "git commit -m '[object Object] no more'",
    ];
    let calls: Vec<Value> = commands.iter().map(|c| bash(c)).collect();
    assert_passes(&calls);
}

#[test]
fn the_shell_denies_reach_the_text_of_each_repl_template() {
    for (code, fix) in [
        ("await $`yes > /dev/null &`;", CAP),
        ("const r = await $`while :; do :; done`.nothrow();", CAP),
        ("await $`rm -rf $HOME/x/*`;", RM_FIX),
        ("await $`bash -c 'rm /tmp/a'`;", RM_FIX),
        ("await $`rg 'foo$' src`.text();", ANCHOR_FIX),
        ("await $`[object Object] > out`;", "pass a string or array"),
        ("await $('rm -f *.log');", RM_FIX),
        ("const a = 1;\nawait $`stress -c 4`;", "line 2"),
        ("await $`echo \\`rm $x\\``;", RM_FIX),
        ("$`cd ${dir} && rm \\${name}`", RM_FIX),
    ] {
        let reason = denied(&repl(code)).unwrap_or_else(|| panic!("{code:?} should be denied"));
        assert!(reason.contains(fix), "{code:?}: {reason}");
        assert!(reason.chars().count() <= 200, "{reason}");
    }
}

#[test]
fn an_interpolation_is_one_literal_word_and_raw_text_is_unknown() {
    let codes = [
        "await $`rm ${path}`;",
        "await $`rm -f ${dir}/a.txt ${other}`;",
        "await $`rg ${'foo$'} src`;",
        "await $`rm ${{raw: x}}`;",
        "await $`rm ${ {raw: '*.log'} }`;",
        "const cmd = 'rm *.log'; await $(cmd);",
        "const s = `rm $x`;",
        "self.log('rm $x, yes > /dev/null');",
        "// await $`rm $x`\nconst a = 1;",
        "await $`git rm -r --cached ${dir}`;",
        "await $`echo \\${HOME}`;",
        "x.$`rm $y`;",
    ];
    let calls: Vec<Value> = codes.iter().map(|c| repl(c)).collect();
    assert_passes(&calls);
}

const WAIT_FIX: &str = "one wait per REPL call, or factory wait lead --any";

#[test]
fn a_repl_script_that_waits_more_than_once_is_denied() {
    let codes = [
        "for (const pr of prs) { await $`factory wait ci ${pr} > out.txt`.timeout(600000); }",
        "for (const pr of prs) await $`factory wait ci ${pr}`;",
        "for (let i = 0; i < 3; i++)\n  await $`factory wait ci 1`.nothrow();",
        "while (true) {\n  const r = await $`factory wait review t1-r1`.nothrow();\n  if (r.exitCode === 0) break;\n}",
        "do { await $`factory wait lead x`; } while (more);",
        "for await (const x of xs) { await $`factory wait ci 1` }",
        "$`bun test`.then(() => $`factory wait ci 3`)",
        "p.catch(async () => { await $`factory wait ci 3` })",
        "p.finally(() => $('factory wait ci 3'))",
        "setInterval(() => $`factory wait ci 3`, 1000)",
        "setTimeout(async () => { await $('factory wait lead x', 600000) }, 10)",
        "prs.forEach(pr => $`factory wait ci ${pr}`)",
        "await Promise.all(prs.map(pr => $`factory wait ci ${pr}`))",
        "await $`factory wait ci 1`;\nawait $`factory wait ci 2`;",
        "await $`factory wait ci 1 && factory wait ci 2`",
        "for (const x of xs) { await $`cd /tmp && bash -c 'factory wait ci 1'` }",
    ];
    let calls: Vec<Value> = codes.iter().map(|c| repl(c)).collect();
    assert_denied_with(&calls, WAIT_FIX);
}

#[test]
fn one_awaited_wait_and_mere_mentions_pass() {
    let codes = [
        "await $`factory wait ci 12 > out.txt`.timeout(600000)",
        "const r = await $`factory wait review t1-r1 > /tmp/w 2>&1`.nothrow().timeout(600000);\nif (r.exitCode === 124) log('again');",
        "for (const f of files) { await $`rg -n x ${f}`; }\nawait $`factory wait ci 3`;",
        "for (const f of files) await $`rg x ${f}`;\nawait $`factory wait ci 3`;",
        "for (const f of files)\n  await $`rg x ${f}`;\nawait $`factory wait ci 3`;",
        "const s = 'factory wait ci 1';\nfor (;;) { log('factory wait ci 2'); break; }",
        "// for (;;) await $`factory wait ci 1`\nawait $`factory wait ci 2`",
        "await $`factory wait ci 3`.then(r => log(r.exitCode))",
        "if (ok) { await $`factory wait ci 3` }",
        "while (more()) { await $`echo factory wait ci 1` }",
        "async function f() { await $`factory wait ci 1` }\nawait f();",
        "const out = await $`factory status`.text();\nwhile (x) { y(); }",
        "xs.map(x => x * 2);\nawait $`factory wait lead --any`;",
        "const t = `for (;;) { factory wait ci 1 }`;",
        "do { x(); } while (more)\nawait $`factory wait ci 1`",
    ];
    let calls: Vec<Value> = codes.iter().map(|c| repl(c)).collect();
    assert_passes(&calls);
}

const CAP: &str = "systemd-run --user --scope -p CPUQuota=200% -- <cmd>";

#[test]
fn a_cpu_burner_on_the_bare_host_is_denied_with_the_capped_scope() {
    let commands = [
        "yes > /dev/null",
        "yes >/dev/null",
        "yes 1>/dev/null 2>&1",
        "yes &>/dev/null",
        "yes >> /dev/null",
        "yes &",
        "yes x &",
        "yes > out.txt &",
        "for i in 1 2 3; do yes > /dev/null & done",
        "while :; do :; done",
        "while true; do :; done",
        "while true; do true; done",
        "until false; do :; done",
        "for ((;;)); do :; done",
        "while :; do :; done &",
        "while true\ndo\n  :\ndone",
        "cd /tmp && while :; do :; done",
        "stress --cpu 8",
        "stress-ng --cpu 0 --timeout 60s",
        "/usr/bin/stress-ng -c 4",
        "timeout 30 stress -c 2",
        "nice stress-ng -c 4",
        "openssl speed",
        "openssl speed -multi 24 rsa",
        "bash -c 'yes > /dev/null'",
        "sh -c \"while :; do :; done\"",
        "systemd-run --user --scope -- stress -c 4",
        "systemd-run --user -p MemoryMax=1G -- yes > /dev/null",
    ];
    let calls: Vec<Value> = commands.iter().map(|c| bash(c)).collect();
    assert_denied_with(&calls, CAP);
}

#[test]
fn a_burner_inside_a_cpu_capped_scope_and_finite_lookalikes_pass() {
    let commands = [
        "systemd-run --user --scope -p CPUQuota=200% -- yes > /dev/null",
        "systemd-run --user --scope -p CPUQuota=200% -- stress-ng -c 4",
        "systemd-run --user --scope --property=CPUQuota=50% -- openssl speed",
        "systemd-run --user --scope -pCPUQuota=100% -- stress -c 1",
        "systemd-run --user --scope -p CPUQuota=200% -- bash -c 'while :; do :; done'",
        "yes | head -3",
        "yes | sdkmanager --licenses",
        "yes y | apt-get install x",
        "yes | head -3 &",
        "echo yes > /dev/null",
        "yes --help",
        "stress --help",
        "stress-ng --version",
        "openssl version",
        "openssl rand -hex 8",
        "while read -r l; do echo \"$l\"; done < f",
        "while true; do sleep 5; done",
        "while :; do date; sleep 1; done",
        "for i in 1 2 3; do :; done",
        "for ((i=0; i<3; i++)); do :; done",
        "rg stress src",
        "git commit -m 'no more yes > /dev/null'",
        "cargo test stress",
        "man stress-ng",
    ];
    let calls: Vec<Value> = commands.iter().map(|c| bash(c)).collect();
    assert_passes(&calls);
}
